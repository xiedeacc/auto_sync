use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, anyhow, bail};
use filetime::{FileTime, set_file_mtime};
use tracing::{error, info, warn};
use walkdir::{DirEntry, WalkDir};

use crate::core::config::{
    AppConfig, DestinationConfig, SnapshotBackend, SourceGroupConfig, SyncTaskRef,
    machine_id_or_local,
};
use crate::core::machines::{find_machine, rsync_endpoint, rsync_path, ssh_target};
use crate::core::state::{Cycle, SnapshotEntry, State};
use crate::core::status::{check_destination_online, check_file_destination_online};

const INTERNAL_TMP: &str = ".auto_sync_tmp";
const INTERNAL_TRASH: &str = ".auto_sync_trash";
const INTERNAL_PROBE: &str = ".auto_sync_probe";

pub fn sync_all_pending(cfg: &AppConfig, state: &mut State) -> Result<()> {
    state.ensure_config(cfg)?;
    loop {
        let mut progressed = false;
        let mut blocked = false;
        for source in cfg.source_groups.iter().filter(|s| s.enabled) {
            let cycles = state.closed_cycles_for_source(&source.id)?;
            for cycle in cycles {
                if state.source_has_target_cycle(&source.id, cycle.id)? {
                    let outcome = sync_cycle_for_source(cfg, state, source, &cycle)?;
                    progressed |= outcome.progressed;
                    blocked |= outcome.blocked;
                } else if cycle.status == "closed" {
                    state.mark_cycle_status(cycle.id, "verified")?;
                }
            }
        }
        if !progressed || !blocked {
            break;
        }
    }
    Ok(())
}

pub fn sync_all_now(cfg: &AppConfig, state: &mut State) -> Result<()> {
    state.force_target_all_destinations(cfg)?;
    sync_all_pending(cfg, state)
}

pub fn sync_source_now(cfg: &AppConfig, state: &mut State, source_id: &str) -> Result<()> {
    state.force_target_source(cfg, source_id)?;
    sync_all_pending(cfg, state)
}

pub fn sync_destination_now(
    cfg: &AppConfig,
    state: &mut State,
    source_id: &str,
    destination_id: &str,
) -> Result<()> {
    state.force_target_destination(cfg, source_id, destination_id)?;
    sync_all_pending(cfg, state)
}

pub fn sync_cycle_for_source(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<SyncCycleOutcome> {
    info!(
        source = source.id,
        cycle_id = cycle.id,
        needs_full_rescan = cycle.needs_full_rescan,
        "sync cycle started"
    );

    if cycle_needs_rsync(state, source, cycle)? {
        return sync_cycle_with_rsync(cfg, state, source, cycle);
    }

    let live_source_endpoint = match SourceEndpoint::resolve(source) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            for dst in &source.destinations {
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    None,
                    "red",
                    "source_unavailable",
                )?;
            }
            state.mark_cycle_status(cycle.id, "failed")?;
            return Err(err)
                .with_context(|| format!("source path is unavailable: {}", source.src.display()));
        }
    };

    let mut all_verified = true;
    let mut targeted_count = 0_usize;
    let mut blocked_count = 0_usize;
    let mut had_unblocked_failure = false;
    let mut progressed = false;
    let mut ready_destinations = Vec::new();
    for (dst_index, dst) in source
        .destinations
        .iter()
        .enumerate()
        .filter(|(_, d)| d.enabled)
    {
        if state.destination_target_cycle(&source.id, &dst.id)? != Some(cycle.id) {
            continue;
        }
        targeted_count += 1;
        let last_verified = state.destination_last_verified(&source.id, &dst.id)?;
        if last_verified >= Some(cycle.id) {
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                Some(cycle.id),
                "green",
                "verified",
            )?;
            continue;
        }

        if let Some(blocker) = sync_order_blocker(cfg, state, &source.id, &dst.id)? {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "red",
                &format!("blocked_by_sync_order:{blocker}"),
            )?;
            continue;
        }

        let dst_endpoint =
            match DestinationEndpoint::resolve(&live_source_endpoint, dst).and_then(|endpoint| {
                endpoint.check_online()?;
                Ok(endpoint)
            }) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    all_verified = false;
                    had_unblocked_failure = true;
                    warn!(
                        source = source.id,
                        destination = dst.id,
                        path = %dst.path.display(),
                        error = %err,
                        "destination offline"
                    );
                    state.upsert_destination_status(
                        &source.id,
                        &dst.id,
                        None,
                        "red",
                        &short_reason(&err),
                    )?;
                    continue;
                }
            };
        ready_destinations.push((dst_index, dst_endpoint));
    }

    if ready_destinations.is_empty() {
        if targeted_count == 0 || all_verified {
            state.mark_cycle_status(cycle.id, "verified")?;
        } else if blocked_count > 0 && !had_unblocked_failure {
            state.mark_cycle_status(cycle.id, "closed")?;
        } else {
            state.mark_cycle_status(cycle.id, "failed")?;
        }
        return Ok(SyncCycleOutcome {
            progressed: false,
            blocked: blocked_count > 0,
        });
    }

    let source_view = SourceReadView::prepare(source, &live_source_endpoint, cycle.id)?;
    let source_endpoint = source_view.endpoint.clone();

    state.mark_cycle_status(cycle.id, "planning")?;
    let source_snapshot = source_endpoint
        .snapshot(&source.excludes)
        .with_context(|| format!("failed to snapshot source {}", source.src.display()))?;
    state.replace_snapshot(cycle.id, &source.id, &source_snapshot)?;

    state.mark_cycle_status(cycle.id, "syncing")?;
    for (dst_index, dst_endpoint) in ready_destinations {
        let dst = &source.destinations[dst_index];
        match sync_endpoint(
            &source_endpoint,
            &dst_endpoint,
            cycle.id,
            &source_snapshot,
            &source.excludes,
        ) {
            Ok(()) => {
                progressed = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    Some(cycle.id),
                    "green",
                    "verified",
                )?;
                info!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    "destination verified"
                );
            }
            Err(err) => {
                all_verified = false;
                had_unblocked_failure = true;
                error!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    error = %err,
                    "destination sync failed"
                );
                let changing_paths = source_changed_paths(&err);
                if changing_paths.is_empty() {
                    state.clear_destination_issues(&source.id, &dst.id)?;
                    state.upsert_destination_status(
                        &source.id,
                        &dst.id,
                        None,
                        "red",
                        &short_reason(&err),
                    )?;
                } else {
                    state.replace_destination_issues(
                        &source.id,
                        &dst.id,
                        cycle.id,
                        "source_changing",
                        &changing_paths,
                        "source file changed while copying",
                    )?;
                    state.upsert_destination_status(
                        &source.id,
                        &dst.id,
                        None,
                        "yellow",
                        "source_changing",
                    )?;
                }
            }
        }
    }

    if targeted_count == 0 || all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
        source_view.cleanup(source);
    } else if blocked_count > 0 && !had_unblocked_failure {
        state.mark_cycle_status(cycle.id, "closed")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(SyncCycleOutcome {
        progressed,
        blocked: blocked_count > 0,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncCycleOutcome {
    pub progressed: bool,
    pub blocked: bool,
}

fn sync_order_blocker(
    cfg: &AppConfig,
    state: &State,
    source_id: &str,
    destination_id: &str,
) -> Result<Option<String>> {
    let task = SyncTaskRef {
        source_id: source_id.to_string(),
        destination_id: destination_id.to_string(),
    };
    for predecessor in sync_order_predecessors(cfg, &task) {
        if !sync_order_predecessor_satisfied(state, &predecessor)? {
            return Ok(Some(sync_task_label(&predecessor)));
        }
    }
    Ok(None)
}

fn sync_order_predecessor_satisfied(state: &State, task: &SyncTaskRef) -> Result<bool> {
    let offset = state.destination_offset(&task.source_id, &task.destination_id)?;
    if let Some(target) = offset.target_cycle_id {
        return Ok(offset.last_verified_cycle_id >= Some(target) && offset.status == "green");
    }
    Ok(offset.last_verified_cycle_id.is_some() && offset.status == "green")
}

fn sync_order_predecessors(cfg: &AppConfig, task: &SyncTaskRef) -> Vec<SyncTaskRef> {
    let mut out = Vec::new();
    let mut stack: Vec<SyncTaskRef> = cfg
        .sync_order
        .iter()
        .filter(|rule| rule.after == *task)
        .map(|rule| rule.before.clone())
        .collect();
    while let Some(predecessor) = stack.pop() {
        if out.contains(&predecessor) {
            continue;
        }
        stack.extend(
            cfg.sync_order
                .iter()
                .filter(|rule| rule.after == predecessor)
                .map(|rule| rule.before.clone()),
        );
        out.push(predecessor);
    }
    out
}

fn sync_task_label(task: &SyncTaskRef) -> String {
    format!("{}:{}", task.source_id, task.destination_id)
}

fn cycle_needs_rsync(state: &State, source: &SourceGroupConfig, cycle: &Cycle) -> Result<bool> {
    if machine_id_or_local(&source.machine_id) != "local" {
        return Ok(true);
    }
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if state.destination_target_cycle(&source.id, &dst.id)? == Some(cycle.id)
            && machine_id_or_local(&dst.machine_id) != "local"
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sync_cycle_with_rsync(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<SyncCycleOutcome> {
    info!(
        source = source.id,
        cycle_id = cycle.id,
        "rsync cycle started"
    );
    let mut all_verified = true;
    let mut targeted_count = 0_usize;
    let mut blocked_count = 0_usize;
    let mut had_unblocked_failure = false;
    let mut progressed = false;

    state.mark_cycle_status(cycle.id, "syncing")?;
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if state.destination_target_cycle(&source.id, &dst.id)? != Some(cycle.id) {
            continue;
        }
        targeted_count += 1;
        let last_verified = state.destination_last_verified(&source.id, &dst.id)?;
        if last_verified >= Some(cycle.id) {
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                Some(cycle.id),
                "green",
                "verified",
            )?;
            continue;
        }
        if let Some(blocker) = sync_order_blocker(cfg, state, &source.id, &dst.id)? {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "red",
                &format!("blocked_by_sync_order:{blocker}"),
            )?;
            continue;
        }

        match sync_rsync_endpoint(cfg, source, dst) {
            Ok(()) => {
                progressed = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    Some(cycle.id),
                    "green",
                    "verified",
                )?;
            }
            Err(err) => {
                all_verified = false;
                had_unblocked_failure = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    None,
                    "red",
                    &short_reason(&err),
                )?;
            }
        }
    }

    if targeted_count == 0 || all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
    } else if blocked_count > 0 && !had_unblocked_failure {
        state.mark_cycle_status(cycle.id, "closed")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(SyncCycleOutcome {
        progressed,
        blocked: blocked_count > 0,
    })
}

fn sync_rsync_endpoint(
    cfg: &AppConfig,
    source: &SourceGroupConfig,
    dst: &DestinationConfig,
) -> Result<()> {
    let source_machine_id = machine_id_or_local(&source.machine_id);
    let dst_machine_id = machine_id_or_local(&dst.machine_id);
    let source_machine = find_machine(cfg, source_machine_id)
        .ok_or_else(|| anyhow!("unknown source machine: {source_machine_id}"))?;
    let dst_machine = find_machine(cfg, dst_machine_id)
        .ok_or_else(|| anyhow!("unknown destination machine: {dst_machine_id}"))?;
    let source_spec = format!("{}/", rsync_endpoint(&source_machine, &source.src));
    let dst_path = dst
        .path
        .join(cross_platform_file_name(&source.src).unwrap_or_else(|| "source".to_string()));
    let dst_spec = format!("{}/", rsync_endpoint(&dst_machine, &dst_path));

    if source_machine_id != "local" && dst_machine_id != "local" {
        return sync_remote_to_remote(&source_machine, &source.src, &dst_machine, &dst_path);
    }

    let mut command = Command::new("rsync");
    command.arg("-a").arg("--delete");
    if source_machine_id != "local" && source_machine.ssh_port != 22 {
        command
            .arg("-e")
            .arg(format!("ssh -p {}", source_machine.ssh_port));
    } else if dst_machine_id != "local" && dst_machine.ssh_port != 22 {
        command
            .arg("-e")
            .arg(format!("ssh -p {}", dst_machine.ssh_port));
    }
    command.arg(source_spec).arg(dst_spec);
    let status = command.status().context("failed to execute rsync")?;
    if !status.success() {
        bail!("rsync failed with status {status}");
    }
    Ok(())
}

fn sync_remote_to_remote(
    source_machine: &crate::core::config::MachineConfig,
    source_path: &Path,
    dst_machine: &crate::core::config::MachineConfig,
    dst_path: &Path,
) -> Result<()> {
    if !source_machine.os.eq_ignore_ascii_case("windows") {
        let source_spec = trailing_rsync_path(rsync_path(source_machine, source_path));
        let dst_spec = trailing_rsync_path(rsync_endpoint(dst_machine, dst_path));
        return run_remote_rsync(
            source_machine,
            &source_spec,
            &dst_spec,
            dst_machine.ssh_port,
        );
    }

    if !dst_machine.os.eq_ignore_ascii_case("windows") {
        let source_spec = trailing_rsync_path(rsync_endpoint(source_machine, source_path));
        let dst_spec = trailing_rsync_path(rsync_path(dst_machine, dst_path));
        return run_remote_rsync(
            dst_machine,
            &source_spec,
            &dst_spec,
            source_machine.ssh_port,
        );
    }

    let source_spec = trailing_rsync_path(rsync_path(source_machine, source_path));
    let dst_spec = trailing_rsync_path(rsync_endpoint(dst_machine, dst_path));
    run_remote_rsync(
        source_machine,
        &source_spec,
        &dst_spec,
        dst_machine.ssh_port,
    )
}

fn run_remote_rsync(
    runner: &crate::core::config::MachineConfig,
    source_spec: &str,
    dst_spec: &str,
    peer_ssh_port: u16,
) -> Result<()> {
    let remote_command = remote_rsync_command(runner, source_spec, dst_spec, peer_ssh_port);
    let mut command = Command::new("ssh");
    if runner.ssh_port != 22 {
        command.arg("-p").arg(runner.ssh_port.to_string());
    }
    command.arg(ssh_target(runner)).arg(remote_command);
    let status = command
        .status()
        .context("failed to execute ssh for remote-to-remote rsync")?;
    if !status.success() {
        bail!("remote-to-remote rsync failed with status {status}");
    }
    Ok(())
}

fn remote_rsync_command(
    runner: &crate::core::config::MachineConfig,
    source_spec: &str,
    dst_spec: &str,
    peer_ssh_port: u16,
) -> String {
    let mut parts = vec![
        "rsync".to_string(),
        "-a".to_string(),
        "--delete".to_string(),
    ];
    if peer_ssh_port != 22 {
        parts.push("-e".to_string());
        parts.push(remote_shell_quote(
            runner,
            &format!("ssh -p {peer_ssh_port}"),
        ));
    }
    parts.push(remote_shell_quote(runner, source_spec));
    parts.push(remote_shell_quote(runner, dst_spec));
    parts.join(" ")
}

fn remote_shell_quote(runner: &crate::core::config::MachineConfig, value: &str) -> String {
    if runner.os.eq_ignore_ascii_case("windows") {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn trailing_rsync_path(mut value: String) -> String {
    if !value.ends_with('/') {
        value.push('/');
    }
    value
}

fn cross_platform_file_name(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim_end_matches(|ch| ch == '/' || ch == '\\');
    let leaf = trimmed
        .rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or(trimmed);
    if leaf.ends_with(':') {
        return None;
    }
    if leaf.is_empty() {
        None
    } else {
        Some(leaf.to_string())
    }
}

#[derive(Debug, Clone)]
struct SourceReadView {
    endpoint: SourceEndpoint,
    zfs_snapshot: Option<ZfsSnapshot>,
}

impl SourceReadView {
    fn prepare(
        source: &SourceGroupConfig,
        live_endpoint: &SourceEndpoint,
        cycle_id: i64,
    ) -> Result<Self> {
        match source.snapshot.backend {
            SnapshotBackend::Manifest => Ok(Self {
                endpoint: live_endpoint.clone(),
                zfs_snapshot: None,
            }),
            SnapshotBackend::Auto | SnapshotBackend::Zfs => {
                match ZfsSnapshot::create(source, cycle_id) {
                    Ok(snapshot) => {
                        let endpoint = match live_endpoint {
                            SourceEndpoint::Dir { .. } => SourceEndpoint::Dir {
                                root: snapshot.source_path.clone(),
                            },
                            SourceEndpoint::File { .. } => SourceEndpoint::File {
                                path: snapshot.source_path.clone(),
                            },
                        };
                        info!(
                            source = source.id,
                            snapshot = snapshot.full_name,
                            path = %snapshot.source_path.display(),
                            "using zfs snapshot source view"
                        );
                        Ok(Self {
                            endpoint,
                            zfs_snapshot: Some(snapshot),
                        })
                    }
                    Err(err) if source.snapshot.backend == SnapshotBackend::Auto => {
                        warn!(
                            source = source.id,
                            error = %err,
                            "zfs snapshot unavailable; falling back to manifest source view"
                        );
                        Ok(Self {
                            endpoint: live_endpoint.clone(),
                            zfs_snapshot: None,
                        })
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn cleanup(&self, source: &SourceGroupConfig) {
        if let Some(snapshot) = &self.zfs_snapshot {
            if let Err(err) = cleanup_zfs_snapshots(source, snapshot) {
                warn!(source = source.id, error = %err, "zfs snapshot cleanup failed");
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ZfsSnapshot {
    dataset: String,
    full_name: String,
    source_path: PathBuf,
}

impl ZfsSnapshot {
    fn create(source: &SourceGroupConfig, cycle_id: i64) -> Result<Self> {
        let dataset = resolve_zfs_dataset(source)?;
        let snapshot_id = format!(
            "{}_{}_{:012}",
            source.snapshot.prefix,
            sanitize_snapshot_component(&source.id),
            cycle_id
        );
        let full_name = format!("{}@{}", dataset.name, snapshot_id);
        ensure_zfs_snapshot(&full_name)?;
        let source_path = dataset
            .mountpoint
            .join(".zfs")
            .join("snapshot")
            .join(&snapshot_id)
            .join(&dataset.path_in_dataset);
        if !source_path.exists() {
            bail!(
                "zfs snapshot path is not visible: {}. Ensure snapdir=visible or use manifest backend",
                source_path.display()
            );
        }
        Ok(Self {
            dataset: dataset.name,
            full_name,
            source_path,
        })
    }
}

#[derive(Debug, Clone)]
struct ZfsDataset {
    name: String,
    mountpoint: PathBuf,
    path_in_dataset: PathBuf,
}

fn resolve_zfs_dataset(source: &SourceGroupConfig) -> Result<ZfsDataset> {
    if let Some(dataset) = &source.snapshot.dataset {
        let mountpoint = zfs_mountpoint(dataset)?;
        let path_in_dataset = source
            .snapshot
            .path_in_dataset
            .clone()
            .unwrap_or_else(|| path_in_dataset(&source.src, &mountpoint).unwrap_or_default());
        return Ok(ZfsDataset {
            name: dataset.clone(),
            mountpoint,
            path_in_dataset,
        });
    }

    let source_path = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    let mut best: Option<(String, PathBuf)> = None;
    for (name, mountpoint) in zfs_filesystems()? {
        if source_path.starts_with(&mountpoint) {
            let replace = best
                .as_ref()
                .map(|(_, current)| mountpoint.components().count() > current.components().count())
                .unwrap_or(true);
            if replace {
                best = Some((name, mountpoint));
            }
        }
    }
    let Some((name, mountpoint)) = best else {
        bail!("source is not on a zfs dataset: {}", source_path.display());
    };
    let path_in_dataset = path_in_dataset(&source_path, &mountpoint)?;
    Ok(ZfsDataset {
        name,
        mountpoint,
        path_in_dataset,
    })
}

fn path_in_dataset(source_path: &Path, mountpoint: &Path) -> Result<PathBuf> {
    let rel = source_path
        .strip_prefix(mountpoint)
        .with_context(|| format!("source is not below mountpoint {}", mountpoint.display()))?;
    if rel.as_os_str().is_empty() {
        Ok(PathBuf::new())
    } else {
        Ok(rel.to_path_buf())
    }
}

fn zfs_filesystems() -> Result<Vec<(String, PathBuf)>> {
    let output = command_stdout(Command::new("zfs").args([
        "list",
        "-H",
        "-t",
        "filesystem",
        "-o",
        "name,mountpoint",
    ]))?;
    let mut filesystems = Vec::new();
    for line in output.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(mountpoint) = parts.next() else {
            continue;
        };
        if mountpoint == "-" {
            continue;
        }
        filesystems.push((name.to_string(), PathBuf::from(mountpoint)));
    }
    Ok(filesystems)
}

fn zfs_mountpoint(dataset: &str) -> Result<PathBuf> {
    let output =
        command_stdout(Command::new("zfs").args(["list", "-H", "-o", "mountpoint", dataset]))?;
    let mountpoint = output.trim();
    if mountpoint.is_empty() || mountpoint == "-" {
        bail!("zfs dataset has no mounted mountpoint: {dataset}");
    }
    Ok(PathBuf::from(mountpoint))
}

fn ensure_zfs_snapshot(full_name: &str) -> Result<()> {
    if Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", full_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
    {
        return Ok(());
    }
    let status = Command::new("zfs")
        .args(["snapshot", full_name])
        .status()
        .context("failed to execute zfs snapshot")?;
    if !status.success() {
        bail!("zfs snapshot failed for {full_name}");
    }
    Ok(())
}

fn cleanup_zfs_snapshots(source: &SourceGroupConfig, latest: &ZfsSnapshot) -> Result<()> {
    let prefix = format!(
        "{}@{}_{}",
        latest.dataset,
        source.snapshot.prefix,
        sanitize_snapshot_component(&source.id)
    );
    let output = command_stdout(Command::new("zfs").args([
        "list",
        "-H",
        "-t",
        "snapshot",
        "-o",
        "name",
        "-s",
        "creation",
        "-r",
        &latest.dataset,
    ]))?;
    let snapshots: Vec<_> = output
        .lines()
        .filter(|name| name.starts_with(&prefix))
        .map(str::to_string)
        .collect();
    let keep = source.snapshot.keep_extra_cycles.saturating_add(1);
    if snapshots.len() <= keep {
        return Ok(());
    }
    for snapshot in &snapshots[..snapshots.len() - keep] {
        let status = Command::new("zfs")
            .args(["destroy", snapshot])
            .status()
            .with_context(|| format!("failed to execute zfs destroy {snapshot}"))?;
        if !status.success() {
            bail!("zfs destroy failed for {snapshot}");
        }
    }
    Ok(())
}

fn command_stdout(command: &mut Command) -> Result<String> {
    let output = command.output().context("failed to execute command")?;
    if !output.status.success() {
        bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn sanitize_snapshot_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
enum SourceEndpoint {
    Dir { root: PathBuf },
    File { path: PathBuf },
}

impl SourceEndpoint {
    fn resolve(source: &SourceGroupConfig) -> Result<Self> {
        let metadata = fs::symlink_metadata(&source.src)
            .with_context(|| format!("failed to read source {}", source.src.display()))?;
        if metadata.is_dir() {
            return Ok(Self::Dir {
                root: source.src.clone(),
            });
        }
        if metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(Self::File {
                path: source.src.clone(),
            });
        }
        bail!("source path is neither a file nor a directory");
    }

    fn snapshot(&self, excludes: &[PathBuf]) -> Result<Vec<SnapshotEntry>> {
        match self {
            Self::Dir { root } => take_snapshot_with_excludes(root, SnapshotMode::Source, excludes),
            Self::File { path } => {
                let rel_path = file_name_string(path)?;
                if is_rel_excluded(Path::new(&rel_path), excludes) {
                    return Ok(Vec::new());
                }
                Ok(vec![snapshot_entry(path, rel_path)?])
            }
        }
    }
}

#[derive(Debug, Clone)]
enum DestinationEndpoint {
    Dir { root: PathBuf },
    File { path: PathBuf },
}

impl DestinationEndpoint {
    fn resolve(source: &SourceEndpoint, dst: &DestinationConfig) -> Result<Self> {
        reject_dangerous_destination(&dst.path)?;
        match source {
            SourceEndpoint::Dir { root: src_root } => {
                if dst.path.exists() && !dst.path.is_dir() {
                    bail!("directory source cannot sync to non-directory destination");
                }
                let dir_name = src_root.file_name().ok_or_else(|| {
                    anyhow::anyhow!("source directory has no name: {}", src_root.display())
                })?;
                Ok(Self::Dir {
                    root: dst.path.join(dir_name),
                })
            }
            SourceEndpoint::File { .. } => {
                if !dst.path.exists() || dst.path.is_dir() {
                    Ok(Self::Dir {
                        root: dst.path.clone(),
                    })
                } else {
                    Ok(Self::File {
                        path: dst.path.clone(),
                    })
                }
            }
        }
    }

    fn check_online(&self) -> Result<()> {
        match self {
            Self::Dir { root } => check_destination_online(root),
            Self::File { path } => check_file_destination_online(path),
        }
    }
}

fn reject_dangerous_destination(path: &Path) -> Result<()> {
    let normalized = normalize_existing_or_raw(path);
    let critical = [
        Path::new("/"),
        Path::new("/dev"),
        Path::new("/proc"),
        Path::new("/sys"),
        Path::new("/run"),
        Path::new("/boot"),
        Path::new("/etc"),
        Path::new("/bin"),
        Path::new("/sbin"),
        Path::new("/usr"),
        Path::new("/lib"),
        Path::new("/lib64"),
    ];
    if critical.iter().any(|critical| {
        if *critical == Path::new("/") {
            normalized == *critical
        } else {
            normalized == *critical || normalized.starts_with(critical)
        }
    }) {
        bail!(
            "refusing to use system path as destination: {}",
            normalized.display()
        );
    }
    Ok(())
}

fn normalize_existing_or_raw(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        let mut out = PathBuf::new();
        for part in path.components() {
            out.push(part.as_os_str());
        }
        out
    })
}

fn sync_endpoint(
    source: &SourceEndpoint,
    dst: &DestinationEndpoint,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
) -> Result<()> {
    match (source, dst) {
        (SourceEndpoint::Dir { root: src_root }, DestinationEndpoint::Dir { root: dst_root }) => {
            sync_destination(src_root, dst_root, cycle_id, source_snapshot, excludes)
        }
        (SourceEndpoint::Dir { .. }, DestinationEndpoint::File { .. }) => {
            bail!("directory source cannot sync to a destination file")
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::Dir { root }) => {
            let rel_path = file_name_string(path)?;
            if is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            sync_file_to_path(path, root, &root.join(rel_path), cycle_id)
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::File { path: dst_path }) => {
            let rel_path = file_name_string(path)?;
            if is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            let parent = dst_path
                .parent()
                .ok_or_else(|| anyhow!("destination file path has no parent"))?;
            sync_file_to_path(path, parent, dst_path, cycle_id)
        }
    }
}

fn sync_destination(
    src_root: &Path,
    dst_root: &Path,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
) -> Result<()> {
    fs::create_dir_all(dst_root).with_context(|| {
        format!(
            "failed to create destination directory: {}",
            dst_root.display()
        )
    })?;
    let result = (|| {
        let mut changing_paths = BTreeSet::new();
        let source_map = map_entries(source_snapshot);
        let dst_snapshot = take_snapshot(dst_root, SnapshotMode::Destination)?;
        let dst_map = map_entries(&dst_snapshot);

        for entry in source_snapshot.iter().filter(|e| e.file_type == "dir") {
            let target = dst_root.join(&entry.rel_path);
            if target.exists() && !target.is_dir() {
                move_to_trash(dst_root, &entry.rel_path, cycle_id)?;
            }
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create directory {}", target.display()))?;
            set_mode(&target, entry.mode).ok();
        }

        for entry in source_snapshot
            .iter()
            .filter(|e| e.file_type == "file" || e.file_type == "symlink")
        {
            let needs_copy = match dst_map.get(&entry.rel_path) {
                Some(existing) => !entries_match(entry, existing),
                None => true,
            };
            if !needs_copy {
                continue;
            }
            if let Err(err) = copy_entry(src_root, dst_root, cycle_id, entry)
                .with_context(|| format!("failed to copy {}", entry.rel_path))
            {
                let paths = source_changed_paths(&err);
                if paths.is_empty() {
                    return Err(err);
                }
                changing_paths.extend(paths);
            }
        }

        let mut extra_paths: Vec<String> = dst_map
            .keys()
            .filter(|rel| {
                !source_map.contains_key(*rel) && !is_rel_excluded(Path::new(rel), excludes)
            })
            .cloned()
            .collect();
        extra_paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
        for rel in extra_paths {
            move_to_trash(dst_root, &rel, cycle_id)
                .with_context(|| format!("failed to remove extra destination path {rel}"))?;
        }

        verify_destination(dst_root, source_snapshot, &changing_paths, excludes)?;
        if !changing_paths.is_empty() {
            return Err(source_changing_error(&changing_paths));
        }
        Ok(())
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    result
}

fn sync_file_to_path(
    src_path: &Path,
    dst_root: &Path,
    final_path: &Path,
    cycle_id: i64,
) -> Result<()> {
    let result = (|| {
        if final_path.exists() && final_path.is_dir() {
            bail!(
                "destination file target is a directory: {}",
                final_path.display()
            );
        }
        let rel_path = file_name_string(final_path)?;
        let entry = snapshot_entry(src_path, rel_path)?;
        let existing = if final_path.exists() || fs::symlink_metadata(final_path).is_ok() {
            Some(snapshot_entry(final_path, entry.rel_path.clone())?)
        } else {
            None
        };
        let needs_copy = existing
            .as_ref()
            .map(|existing| !entries_match(&entry, existing))
            .unwrap_or(true);

        if needs_copy {
            copy_single_entry(src_path, dst_root, cycle_id, &entry, final_path)?;
        }
        verify_file_target(final_path, &entry)?;
        Ok(())
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    result
}

fn copy_single_entry(
    src: &Path,
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent {}", parent.display()))?;
    }
    match entry.file_type.as_str() {
        "file" => copy_file(src, dst_root, cycle_id, entry, final_path),
        "symlink" => copy_symlink(src, dst_root, cycle_id, entry, final_path),
        other => Err(anyhow!("unsupported single source type {other}")),
    }
}

fn verify_file_target(final_path: &Path, expected: &SnapshotEntry) -> Result<()> {
    if !final_path.exists() && fs::symlink_metadata(final_path).is_err() {
        bail!("destination missing {}", final_path.display());
    }
    let actual = snapshot_entry(final_path, expected.rel_path.clone())?;
    if !entries_match(expected, &actual) {
        bail!("destination mismatch at {}", final_path.display());
    }
    Ok(())
}

pub fn take_snapshot(root: &Path, mode: SnapshotMode) -> Result<Vec<SnapshotEntry>> {
    take_snapshot_with_excludes(root, mode, &[])
}

fn take_snapshot_with_excludes(
    root: &Path,
    mode: SnapshotMode,
    excludes: &[PathBuf],
) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    for item in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| should_visit(root, entry, mode, excludes))
    {
        let item = item?;
        let path = item.path();
        if path == root {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("failed to strip root from {}", path.display()))?;
        let rel_path = rel_to_string(rel)?;
        if let Some(entry) = snapshot_entry_if_supported(path, rel_path)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

fn snapshot_entry(path: &Path, rel_path: String) -> Result<SnapshotEntry> {
    snapshot_entry_if_supported(path, rel_path.clone())?
        .ok_or_else(|| anyhow!("unsupported file type at {}", path.display()))
}

fn snapshot_entry_if_supported(path: &Path, rel_path: String) -> Result<Option<SnapshotEntry>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to read metadata {}", path.display()))?;
    let file_type = if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "dir"
    } else if metadata.is_file() {
        "file"
    } else {
        return Ok(None);
    };
    let hash = match file_type {
        "file" => Some(hash_file(path)?),
        "symlink" => Some(hash_symlink(path)?),
        _ => None,
    };
    Ok(Some(SnapshotEntry {
        rel_path,
        file_type: file_type.to_string(),
        size: metadata.len() as i64,
        mtime_ns: metadata_mtime_ns(&metadata)?,
        mode: metadata_mode(&metadata),
        hash,
    }))
}

#[derive(Debug, Clone, Copy)]
pub enum SnapshotMode {
    Source,
    Destination,
}

fn should_visit(root: &Path, entry: &DirEntry, mode: SnapshotMode, excludes: &[PathBuf]) -> bool {
    if matches!(mode, SnapshotMode::Source) {
        return !entry_is_excluded(root, entry.path(), excludes);
    }
    let name = entry.file_name().to_string_lossy();
    name != INTERNAL_TMP && name != INTERNAL_TRASH && name != INTERNAL_PROBE
}

fn entry_is_excluded(root: &Path, path: &Path, excludes: &[PathBuf]) -> bool {
    if path == root {
        return false;
    }
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    is_rel_excluded(rel, excludes)
}

fn is_rel_excluded(rel: &Path, excludes: &[PathBuf]) -> bool {
    excludes
        .iter()
        .any(|exclude| rel == exclude || rel.starts_with(exclude))
}

fn copy_entry(
    src_root: &Path,
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
) -> Result<()> {
    let src = src_root.join(&entry.rel_path);
    let final_path = dst_root.join(&entry.rel_path);
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent {}", parent.display()))?;
    }

    match entry.file_type.as_str() {
        "file" => copy_file(&src, dst_root, cycle_id, entry, &final_path),
        "symlink" => copy_symlink(&src, dst_root, cycle_id, entry, &final_path),
        other => Err(anyhow!("unsupported entry type {other}")),
    }
}

fn copy_file(
    src: &Path,
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    let tmp = tmp_path(dst_root, cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if tmp.exists() {
        remove_any(&tmp)?;
    }
    fs::copy(src, &tmp)
        .with_context(|| format!("failed to copy {} to {}", src.display(), tmp.display()))?;
    let actual_hash = hash_file(&tmp)?;
    if Some(actual_hash) != entry.hash {
        remove_any(&tmp).ok();
        bail!("source changed while copying {}", entry.rel_path);
    }
    set_mode(&tmp, entry.mode).ok();
    let mtime = FileTime::from_unix_time(
        entry.mtime_ns / 1_000_000_000,
        (entry.mtime_ns % 1_000_000_000) as u32,
    );
    set_file_mtime(&tmp, mtime).ok();
    fsync_file(&tmp).ok();
    replace_path(&tmp, final_path)?;
    fsync_parent(final_path).ok();
    Ok(())
}

fn copy_symlink(
    src: &Path,
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    let target =
        fs::read_link(src).with_context(|| format!("failed to read symlink {}", src.display()))?;
    let tmp = tmp_path(dst_root, cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if tmp.exists() {
        remove_any(&tmp)?;
    }
    create_symlink(src, &target, &tmp)
        .with_context(|| format!("failed to create symlink {}", tmp.display()))?;
    if Some(hash_symlink(&tmp)?) != entry.hash {
        remove_any(&tmp).ok();
        bail!("source symlink changed while copying {}", entry.rel_path);
    }
    replace_path(&tmp, final_path)?;
    fsync_parent(final_path).ok();
    Ok(())
}

fn replace_path(tmp: &Path, final_path: &Path) -> Result<()> {
    if final_path.exists() || fs::symlink_metadata(final_path).is_ok() {
        let tmp_meta = fs::symlink_metadata(tmp)?;
        let final_meta = fs::symlink_metadata(final_path)?;
        let compatible = (tmp_meta.is_file() && final_meta.is_file())
            || (tmp_meta.file_type().is_symlink() && final_meta.file_type().is_symlink());
        if !compatible {
            remove_any(final_path)?;
        }
    }
    fs::rename(tmp, final_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

fn move_to_trash(dst_root: &Path, rel: &str, cycle_id: i64) -> Result<()> {
    let path = dst_root.join(rel);
    if !path.exists() && fs::symlink_metadata(&path).is_err() {
        return Ok(());
    }
    let trash = dst_root
        .join(INTERNAL_TRASH)
        .join(cycle_id.to_string())
        .join(rel);
    if let Some(parent) = trash.parent() {
        fs::create_dir_all(parent)?;
    }
    if trash.exists() || fs::symlink_metadata(&trash).is_ok() {
        remove_any(&trash)?;
    }
    match fs::rename(&path, &trash) {
        Ok(()) => Ok(()),
        Err(_) => {
            remove_any(&path)?;
            Ok(())
        }
    }
}

fn verify_destination(
    dst_root: &Path,
    source_snapshot: &[SnapshotEntry],
    ignored_paths: &BTreeSet<String>,
    excludes: &[PathBuf],
) -> Result<()> {
    let expected = map_entries(source_snapshot);
    let actual_snapshot = take_snapshot(dst_root, SnapshotMode::Destination)?;
    let actual = map_entries(&actual_snapshot);
    for (rel, want) in &expected {
        if ignored_paths.contains(rel) {
            continue;
        }
        match actual.get(rel) {
            Some(got) if entries_match(want, got) => {}
            Some(_) => bail!("destination mismatch at {rel}"),
            None => bail!("destination missing {rel}"),
        }
    }
    for rel in actual.keys() {
        if is_rel_excluded(Path::new(rel), excludes) {
            continue;
        }
        if !expected.contains_key(rel) {
            bail!("destination has extra path {rel}");
        }
    }
    Ok(())
}

fn entries_match(left: &SnapshotEntry, right: &SnapshotEntry) -> bool {
    if left.file_type != right.file_type {
        return false;
    }
    match left.file_type.as_str() {
        "dir" => true,
        "file" => left.size == right.size && left.hash == right.hash,
        "symlink" => left.hash == right.hash,
        _ => false,
    }
}

fn map_entries(entries: &[SnapshotEntry]) -> BTreeMap<String, SnapshotEntry> {
    entries
        .iter()
        .map(|entry| (entry.rel_path.clone(), entry.clone()))
        .collect()
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0_u8; 1024 * 64];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_symlink(path: &Path) -> Result<String> {
    let target = fs::read_link(path)?;
    Ok(format!("symlink:{}", target.to_string_lossy()))
}

fn tmp_path(dst_root: &Path, cycle_id: i64, rel: &str) -> PathBuf {
    let file_name = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "entry".to_string());
    let parent = Path::new(rel).parent().unwrap_or_else(|| Path::new(""));
    dst_root
        .join(INTERNAL_TMP)
        .join(cycle_id.to_string())
        .join(parent)
        .join(format!("{file_name}.tmp.{}", std::process::id()))
}

fn remove_any(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    set_permissions_mode(&mut perms, mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn metadata_mtime_ns(metadata: &fs::Metadata) -> Result<i64> {
    let modified = metadata.modified()?;
    let ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).context("mtime seconds overflow")?;
            secs.checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(i64::from(duration.subsec_nanos())))
                .ok_or_else(|| anyhow!("mtime nanoseconds overflow"))?
        }
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).context("mtime seconds overflow")?;
            let ns = secs
                .checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(i64::from(duration.subsec_nanos())))
                .ok_or_else(|| anyhow!("mtime nanoseconds overflow"))?;
            -ns
        }
    };
    Ok(ns)
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    metadata.mode()
}

#[cfg(windows)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        0o120777
    } else if metadata.is_dir() {
        0o040755
    } else if metadata.permissions().readonly() {
        0o100444
    } else {
        0o100644
    }
}

#[cfg(unix)]
fn set_permissions_mode(perms: &mut fs::Permissions, mode: u32) {
    perms.set_mode(mode);
}

#[cfg(windows)]
fn set_permissions_mode(perms: &mut fs::Permissions, mode: u32) {
    perms.set_readonly(mode & 0o222 == 0);
}

#[cfg(unix)]
fn create_symlink(_src: &Path, target: &Path, tmp: &Path) -> io::Result<()> {
    symlink(target, tmp)
}

#[cfg(windows)]
fn create_symlink(src: &Path, target: &Path, tmp: &Path) -> io::Result<()> {
    if fs::metadata(src).map(|meta| meta.is_dir()).unwrap_or(false) {
        symlink_dir(target, tmp)
    } else {
        symlink_file(target, tmp)
    }
}

fn fsync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn fsync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn rel_to_string(path: &Path) -> Result<String> {
    let value = path.to_string_lossy().to_string();
    if value.is_empty() || value == "." {
        bail!("invalid empty relative path");
    }
    Ok(value)
}

fn file_name_string(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
    let value = name.to_string_lossy().to_string();
    if value.is_empty() {
        bail!("path has empty file name: {}", path.display());
    }
    Ok(value)
}

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn short_reason(err: &anyhow::Error) -> String {
    let text = err
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    text.chars().take(120).collect()
}

fn source_changed_paths(err: &anyhow::Error) -> Vec<String> {
    let mut paths = BTreeSet::new();
    let prefixes = [
        "source changed while copying ",
        "source symlink changed while copying ",
    ];
    for cause in err.chain() {
        let text = cause.to_string();
        for line in text.lines() {
            for prefix in prefixes {
                if let Some((_, path)) = line.split_once(prefix) {
                    let path = path.trim();
                    if !path.is_empty() {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
    paths.into_iter().collect()
}

fn source_changing_error(paths: &BTreeSet<String>) -> anyhow::Error {
    anyhow!(
        "{}",
        paths
            .iter()
            .map(|path| format!("source changed while copying {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn cleanup_tmp_cycle(dst_root: &Path, cycle_id: i64) {
    let root = dst_root.join(INTERNAL_TMP);
    let path = root.join(cycle_id.to_string());
    if path.exists() {
        remove_any(&path).ok();
    }
    fs::remove_dir(&root).ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{
        AppConfig, DestinationConfig, MachineConfig, ScheduleConfig, SnapshotBackend,
        SnapshotConfig, SourceGroupConfig, SyncMode, SyncOrderRule, SyncTaskRef,
    };
    use crate::core::state::State;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn extracts_file_name_from_windows_paths() {
        assert_eq!(
            cross_platform_file_name(Path::new("C:\\Users\\me\\blog")),
            Some("blog".to_string())
        );
        assert_eq!(
            cross_platform_file_name(Path::new("D:\\data\\source\\")),
            Some("source".to_string())
        );
        assert_eq!(cross_platform_file_name(Path::new("C:\\")), None);
    }

    #[test]
    fn builds_remote_to_remote_rsync_command() {
        let mut runner = MachineConfig::local();
        runner.id = "nas-a".to_string();
        runner.os = "linux".to_string();
        let command = remote_rsync_command(&runner, "/src/data/", "root@nas-b:/dst/data/", 10022);
        assert_eq!(
            command,
            "rsync -a --delete -e 'ssh -p 10022' '/src/data/' 'root@nas-b:/dst/data/'"
        );
    }

    #[test]
    fn syncs_source_file_to_destination_directory() {
        let temp = temp_dir("file_to_dir");
        let src = temp.join("source.txt");
        let dst = temp.join("dst");
        fs::create_dir_all(&dst).unwrap();
        fs::write(&src, b"hello").unwrap();

        sync_endpoint(
            &SourceEndpoint::File { path: src.clone() },
            &DestinationEndpoint::Dir { root: dst.clone() },
            7,
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(fs::read(dst.join("source.txt")).unwrap(), b"hello");
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn syncs_source_file_to_destination_file() {
        let temp = temp_dir("file_to_file");
        let src = temp.join("source.txt");
        let dst = temp.join("renamed.txt");
        fs::write(&src, b"hello").unwrap();
        fs::write(&dst, b"old").unwrap();

        sync_endpoint(
            &SourceEndpoint::File { path: src.clone() },
            &DestinationEndpoint::File { path: dst.clone() },
            7,
            &[],
            &[],
        )
        .unwrap();

        assert_eq!(fs::read(dst).unwrap(), b"hello");
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn rejects_directory_source_to_destination_file() {
        let temp = temp_dir("dir_to_file");
        let src = temp.join("src");
        let dst = temp.join("dst-file");
        fs::create_dir_all(&src).unwrap();
        fs::write(&dst, b"old").unwrap();

        let result = DestinationEndpoint::resolve(
            &SourceEndpoint::Dir { root: src },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
            },
        );

        assert!(result.is_err());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn rejects_system_path_destination() {
        let result = DestinationEndpoint::resolve(
            &SourceEndpoint::Dir {
                root: PathBuf::from("/tmp/source"),
            },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/dev"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn treats_missing_destination_path_as_directory_for_source_file() {
        let temp = temp_dir("missing_dst_is_dir");
        let src = temp.join("source.txt");
        let dst = temp.join("new-dir");
        fs::write(&src, b"hello").unwrap();

        let endpoint = DestinationEndpoint::resolve(
            &SourceEndpoint::File { path: src },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
            },
        )
        .unwrap();

        assert!(matches!(endpoint, DestinationEndpoint::Dir { .. }));
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_all_now_sets_target_and_verifies_destination() {
        let temp = temp_dir("sync_all_now");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();
        assert_eq!(
            fs::read(dst.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );

        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].status, "green");
        assert_eq!(views[0].target_cycle_id, views[0].last_verified_cycle_id);
        assert!(views[0].target_cycle_id.is_some());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_destination_now_only_targets_selected_destination() {
        let temp = temp_dir("sync_one_dst_now");
        let src = temp.join("src");
        let dst_1 = temp.join("dst_1");
        let dst_2 = temp.join("dst_2");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst_1).unwrap();
        fs::create_dir_all(&dst_2).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![
                DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_1.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_2.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
            ],
        });

        let mut state = State::open(&db).unwrap();
        sync_destination_now(&cfg, &mut state, "src_1", "dst_2").unwrap();

        assert!(!dst_1.join("src").join("hello.txt").exists());
        assert_eq!(
            fs::read(dst_2.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );

        let views = state.destination_views(&cfg).unwrap();
        let first = views
            .iter()
            .find(|view| view.destination_id == "dst_1")
            .unwrap();
        let second = views
            .iter()
            .find(|view| view.destination_id == "dst_2")
            .unwrap();
        assert_eq!(first.target_cycle_id, None);
        assert_eq!(first.last_verified_cycle_id, None);
        assert_eq!(second.status, "green");
        assert_eq!(second.target_cycle_id, second.last_verified_cycle_id);

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn excludes_source_paths_without_deleting_existing_destination_paths() {
        let temp = temp_dir("exclude_paths");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(src.join("skip_dir")).unwrap();
        // effective dst root is dst/src because the source dir is named "src"
        fs::create_dir_all(dst.join("src").join("skip_dir")).unwrap();
        fs::write(src.join("keep.txt"), b"keep").unwrap();
        fs::write(src.join("skip.txt"), b"skip source").unwrap();
        fs::write(src.join("skip_dir").join("nested.txt"), b"skip nested").unwrap();
        fs::write(dst.join("src").join("skip.txt"), b"existing skip").unwrap();
        fs::write(
            dst.join("src").join("skip_dir").join("nested.txt"),
            b"existing nested",
        )
        .unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            excludes: vec![PathBuf::from("skip.txt"), PathBuf::from("skip_dir")],
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        let eff = dst.join("src");
        assert_eq!(fs::read(eff.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(eff.join("skip.txt")).unwrap(), b"existing skip");
        assert_eq!(
            fs::read(eff.join("skip_dir").join("nested.txt")).unwrap(),
            b"existing nested"
        );
        assert!(!eff.join(".auto_sync_trash").exists());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_order_blocks_after_task_until_before_task_verifies() {
        let temp = temp_dir("sync_order_blocks");
        let before_src = temp.join("before_src");
        let after_src = temp.join("after_src");
        let before_dst = temp.join("before_dst_file");
        let after_dst = temp.join("after_dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&before_src).unwrap();
        fs::create_dir_all(&after_src).unwrap();
        fs::create_dir_all(&after_dst).unwrap();
        fs::write(before_src.join("before.txt"), b"before").unwrap();
        fs::write(after_src.join("after.txt"), b"after").unwrap();
        fs::write(&before_dst, b"not a directory").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "after_src".to_string(),
            machine_id: "local".to_string(),
            src: after_src.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "after_dst".to_string(),
                machine_id: "local".to_string(),
                path: after_dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        cfg.source_groups.push(SourceGroupConfig {
            id: "before_src".to_string(),
            machine_id: "local".to_string(),
            src: before_src.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "before_dst".to_string(),
                machine_id: "local".to_string(),
                path: before_dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "before_src".to_string(),
                destination_id: "before_dst".to_string(),
            },
            after: SyncTaskRef {
                source_id: "after_src".to_string(),
                destination_id: "after_dst".to_string(),
            },
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        assert!(!after_dst.join("after_src").join("after.txt").exists());
        let views = state.destination_views(&cfg).unwrap();
        let after_view = views
            .iter()
            .find(|view| view.source_id == "after_src")
            .unwrap();
        assert!(
            after_view
                .status_reason
                .starts_with("blocked_by_sync_order")
        );

        fs::remove_file(&before_dst).unwrap();
        fs::create_dir_all(&before_dst).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        assert_eq!(
            fs::read(before_dst.join("before_src").join("before.txt")).unwrap(),
            b"before"
        );
        assert_eq!(
            fs::read(after_dst.join("after_src").join("after.txt")).unwrap(),
            b"after"
        );

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn extracts_source_changed_paths_from_error_chain() {
        let err = anyhow!(
            "source changed while copying log/live.log\nsource changed while copying log/other.log"
        )
        .context("failed to copy log/live.log");

        assert_eq!(
            source_changed_paths(&err),
            vec!["log/live.log".to_string(), "log/other.log".to_string()]
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("auto_sync_{name}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
