//! File collector: pull files/directories from arbitrary SSH hosts into a
//! single local git repository, track oversized files with Git LFS, then commit
//! and push.
//!
//! Everything shells out to the system `ssh`/`scp`/`git` — there is no SSH
//! client crate — so it works with the built-in OpenSSH on Windows and the
//! stock tools on Linux. Each host is described by structured fields
//! (`HostName`/`User`/`Port`/`IdentityFile`), and the engine builds the
//! `ssh`/`scp` commands with explicit `-i`/`-p` flags — no ssh config file.
//! The whole feature is UI-driven (the "Collector" button): nothing here runs
//! on a schedule.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::core::config::{CollectorConfig, CollectorHost};

/// Legacy marker inserted between a split file's name and its part index, e.g.
/// `big.bin.autosplit.000`. New runs use Git LFS, but old parts are cleaned up.
const AUTOSPLIT_MARKER: &str = ".autosplit.";

/// Per-host permission cache written under the host's local root at collect
/// time and replayed by `deploy` (Windows can't preserve Unix modes).
const PERMS_FILE: &str = ".auto_sync_perms";
const NAME_MANIFEST_FILE: &str = ".auto_sync_name_manifest.json";
const ENCODED_NAME_DIR: &str = ".auto_sync_name";

/// Non-interactive ssh/scp options: never prompt for a password, auto-trust a
/// new host key (LAN/known infra), and cap the connect wait so a dead host
/// fails fast instead of hanging the run.
const SSH_OPTS: &[&str] = &[
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ConnectTimeout=15",
];

static GIT_FINALIZE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Live state of a collector run, polled by the UI.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CollectorRunState {
    pub running: bool,
    pub started_at: Option<String>,
    pub started_epoch_ms: Option<i64>,
    pub finished_at: Option<String>,
    pub duration_ms: Option<u64>,
    /// None while running; Some(true/false) once finished.
    pub ok: Option<bool>,
    pub current_file: Option<String>,
    pub total_files: usize,
    pub succeeded_files: usize,
    pub failed_files: usize,
    pub errors: Vec<CollectorRunIssue>,
    pub log: Vec<String>,
}

/// Structured details for the UI's Collector Error dialog.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CollectorRunIssue {
    pub kind: String,
    pub host: String,
    pub path: String,
    pub message: String,
}

/// One entry returned by the remote path browser.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectorBrowseEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
}

/// Result of browsing a remote directory over ssh.
#[derive(Debug, Clone, Serialize)]
pub struct CollectorBrowseResponse {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<CollectorBrowseEntry>,
}

/// The connection parameters a browse request carries (a subset of
/// `CollectorHost`). Sent by the UI so it can browse a host before it is saved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CollectorBrowseRequest {
    #[serde(default)]
    pub hostname: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub identity_file: String,
    #[serde(default)]
    pub path: String,
}

/// SSH connection view shared by run and browse.
struct SshConn<'a> {
    hostname: &'a str,
    user: &'a str,
    port: u16,
    identity_file: &'a str,
}

impl<'a> SshConn<'a> {
    fn from_host(host: &'a CollectorHost) -> Self {
        Self {
            hostname: host.hostname.trim(),
            user: host.user.trim(),
            port: host.port,
            identity_file: host.identity_file.trim(),
        }
    }

    fn from_request(req: &'a CollectorBrowseRequest) -> Self {
        Self {
            hostname: req.hostname.trim(),
            user: req.user.trim(),
            port: req.port,
            identity_file: req.identity_file.trim(),
        }
    }

    /// `user@host` (or just `host` when no user is set).
    fn dest(&self) -> String {
        if self.user.is_empty() {
            self.hostname.to_string()
        } else {
            format!("{}@{}", self.user, self.hostname)
        }
    }
}

fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    cmd
}

/// Expand a leading `~` / `~/` to the user's home directory (ssh does this for
/// config files, but we pass identity paths as explicit `-i` args).
fn expand_tilde(path: &str) -> String {
    let path = path.trim();
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_default();
        if !home.is_empty() {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

/// Build an `ssh` command for `conn` (port via `-p`, key via `-i`).
fn ssh_command(conn: &SshConn) -> Command {
    let mut cmd = command("ssh");
    if conn.port != 0 {
        cmd.arg("-p").arg(conn.port.to_string());
    }
    if !conn.identity_file.is_empty() {
        cmd.arg("-i").arg(expand_tilde(conn.identity_file));
    }
    cmd.args(SSH_OPTS);
    cmd
}

/// Build an `scp` command for `conn` (port via `-P`, key via `-i`).
fn scp_command(conn: &SshConn) -> Command {
    let mut cmd = command("scp");
    if conn.port != 0 {
        cmd.arg("-P").arg(conn.port.to_string());
    }
    if !conn.identity_file.is_empty() {
        cmd.arg("-i").arg(expand_tilde(conn.identity_file));
    }
    cmd.args(SSH_OPTS);
    cmd
}

fn log(state: &Arc<Mutex<CollectorRunState>>, msg: impl Into<String>) {
    let msg = msg.into();
    let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg);
    tracing::info!(target: "collector", "{}", msg);
    if let Ok(mut guard) = state.lock() {
        guard.log.push(line);
    }
}

fn set_total_files(state: &Arc<Mutex<CollectorRunState>>, total: usize) {
    if let Ok(mut guard) = state.lock() {
        guard.total_files = total;
    }
}

fn set_current_file(state: &Arc<Mutex<CollectorRunState>>, path: Option<&str>) {
    if let Ok(mut guard) = state.lock() {
        guard.current_file = path.map(str::to_string);
    }
}

fn record_file_ok(state: &Arc<Mutex<CollectorRunState>>) {
    if let Ok(mut guard) = state.lock() {
        guard.succeeded_files = guard.succeeded_files.saturating_add(1);
    }
}

fn record_file_failed(state: &Arc<Mutex<CollectorRunState>>, count: usize) {
    if let Ok(mut guard) = state.lock() {
        guard.failed_files = guard.failed_files.saturating_add(count);
    }
}

fn record_run_issue(
    state: &Arc<Mutex<CollectorRunState>>,
    kind: &str,
    host: &str,
    path: &str,
    message: impl Into<String>,
) {
    if let Ok(mut guard) = state.lock() {
        guard.errors.push(CollectorRunIssue {
            kind: kind.to_string(),
            host: host.to_string(),
            path: path.to_string(),
            message: message.into(),
        });
    }
}

fn finish_state(state: &Arc<Mutex<CollectorRunState>>, ok: bool) {
    if let Ok(mut guard) = state.lock() {
        let now = chrono::Local::now();
        guard.running = false;
        guard.ok = Some(ok);
        guard.current_file = None;
        guard.finished_at = Some(now.format("%Y-%m-%d %H:%M:%S").to_string());
        guard.duration_ms = guard
            .started_epoch_ms
            .map(|started| now.timestamp_millis().saturating_sub(started).max(0) as u64);
    }
}

/// Entry point for a background run. Drives the whole pipeline and records the
/// final status into `state`. Never panics — every failure becomes a log line.
pub fn run(cfg: CollectorConfig, state: Arc<Mutex<CollectorRunState>>) {
    let ok = match run_inner(&cfg, &state) {
        Ok(0) => {
            log(&state, "Collector finished: all paths pulled.");
            true
        }
        Ok(failures) => {
            log(
                &state,
                format!("Collector finished with {failures} failed path(s)."),
            );
            false
        }
        Err(err) => {
            log(&state, format!("ERROR: {err:#}"));
            false
        }
    };
    finish_state(&state, ok);
}

pub fn run_host(cfg: CollectorConfig, host_index: usize, state: Arc<Mutex<CollectorRunState>>) {
    let ok = match run_host_inner(&cfg, host_index, &state) {
        Ok(0) => {
            log(&state, "Collector finished: all paths pulled.");
            true
        }
        Ok(failures) => {
            log(
                &state,
                format!("Collector finished with {failures} failed path(s)."),
            );
            false
        }
        Err(err) => {
            log(&state, format!("ERROR: {err:#}"));
            false
        }
    };
    finish_state(&state, ok);
}

/// Runs the pipeline, returning the number of remote paths that failed to
/// pull (a hard/setup error still returns `Err`).
fn run_inner(cfg: &CollectorConfig, state: &Arc<Mutex<CollectorRunState>>) -> Result<usize> {
    let enabled: Vec<&CollectorHost> = cfg.hosts.iter().filter(|h| h.enabled).collect();
    if enabled.is_empty() {
        bail!("no enabled hosts to pull from");
    }
    run_hosts_inner(cfg, enabled, state)
}

fn run_host_inner(
    cfg: &CollectorConfig,
    host_index: usize,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<usize> {
    let host = cfg
        .hosts
        .get(host_index)
        .ok_or_else(|| anyhow::anyhow!("no host at index {host_index}"))?;
    run_hosts_inner(cfg, vec![host], state)
}

fn run_hosts_inner(
    cfg: &CollectorConfig,
    hosts: Vec<&CollectorHost>,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<usize> {
    let git_dir = cfg.git_dir.clone();
    if git_dir.as_os_str().is_empty() {
        bail!("git repository directory is not set");
    }
    fs::create_dir_all(&git_dir)
        .with_context(|| format!("creating git dir {}", git_dir.display()))?;
    if !git_dir.join(".git").exists() {
        log(state, format!("git init {}", git_dir.display()));
        run_git(&git_dir, &["init"], state)?;
    }

    let total_files = hosts.iter().map(|host| count_collect_paths(host)).sum();
    set_total_files(state, total_files);

    let mut failures = 0;
    for host in hosts {
        failures += pull_host(host, state);
    }
    set_current_file(state, None);

    if cfg.split_threshold_mb > 0 || cfg.auto_commit_push {
        let _git_guard = GIT_FINALIZE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| anyhow::anyhow!("collector git finalize lock poisoned"))?;
        if cfg.split_threshold_mb > 0 {
            track_large_files_with_lfs(&git_dir, cfg.split_threshold_mb, state)?;
        }

        if cfg.auto_commit_push {
            commit_and_push(&git_dir, state)?;
        } else {
            log(
                state,
                "auto commit/push disabled; leaving working tree as-is",
            );
        }
    } else {
        log(
            state,
            "auto commit/push disabled; leaving working tree as-is",
        );
    }

    Ok(failures)
}

fn count_collect_paths(host: &CollectorHost) -> usize {
    collect_targets(host).len()
}

fn collect_targets(host: &CollectorHost) -> Vec<String> {
    let excludes = normalize_excludes(&host.exclude);
    host.paths
        .iter()
        .map(|path| path.trim_end_matches('/').trim())
        .filter(|path| !path.is_empty() && !is_excluded(path, &excludes))
        .map(str::to_string)
        .collect()
}

fn record_host_setup_failure(
    state: &Arc<Mutex<CollectorRunState>>,
    host: &CollectorHost,
    label: &str,
    message: &str,
) -> usize {
    let targets = collect_targets(host);
    if targets.is_empty() {
        record_run_issue(state, "host_failed", label, "", message);
        return 0;
    }
    for target in &targets {
        record_run_issue(state, "host_failed", label, target, message);
    }
    targets.len()
}

/// Pull every configured path for one host, returning the failure count. Host
/// setup problems degrade to logged failures rather than aborting the run.
fn pull_host(host: &CollectorHost, state: &Arc<Mutex<CollectorRunState>>) -> usize {
    let label = if host.name.trim().is_empty() {
        host.hostname.clone()
    } else {
        host.name.clone()
    };
    if host.hostname.trim().is_empty() {
        let message = "no hostname, skipping";
        log(state, format!("host '{label}': {message}"));
        let failed = record_host_setup_failure(state, host, &label, message);
        record_file_failed(state, failed);
        return failed;
    }
    if host.root.as_os_str().is_empty() {
        let message = "no local root, skipping";
        log(state, format!("host '{label}': {message}"));
        let failed = record_host_setup_failure(state, host, &label, message);
        record_file_failed(state, failed);
        return failed;
    }
    let conn = SshConn::from_host(host);
    log(state, format!("=== host {label} ({}) ===", conn.dest()));

    let mut failures = 0;
    let mut pulled = Vec::new();
    let excludes = normalize_excludes(&host.exclude);
    let targets = collect_targets(host);
    if let Err(err) = ssh_capture(&conn, "true") {
        let message = format!("host connection failed: {err:#}");
        log(state, format!("host '{label}': {message}"));
        for target in &targets {
            record_run_issue(state, "host_failed", &label, target, message.clone());
        }
        let failed = targets.len();
        record_file_failed(state, failed);
        return failed;
    }
    if targets.is_empty() {
        return 0;
    }
    set_current_file(
        state,
        Some(&format!("{} path(s) from {label}", targets.len())),
    );
    let mut files = Vec::new();
    let mut symlink_files = Vec::new();
    let mut dirs = Vec::new();
    for remote in &targets {
        set_current_file(state, Some(remote));
        match remote_path_kind(&conn, remote) {
            Ok(RemotePathKind::File) => files.push(remote.clone()),
            Ok(RemotePathKind::Dir) => dirs.push(remote.clone()),
            Ok(RemotePathKind::SymlinkFile) => symlink_files.push(remote.clone()),
            Err(err) => {
                failures += 1;
                record_file_failed(state, 1);
                record_run_issue(state, "file_failed", &label, remote, format!("{err:#}"));
                log(state, format!("  FAILED {remote}: {err:#}"));
            }
        }
    }

    if !symlink_files.is_empty() {
        set_current_file(
            state,
            Some(&format!(
                "{} symlink file(s) from {label}",
                symlink_files.len()
            )),
        );
        for remote in &symlink_files {
            set_current_file(state, Some(remote));
            match pull_paths_tar(host, &conn, std::slice::from_ref(remote), &excludes, state) {
                Ok(()) => {
                    record_file_ok(state);
                    log(state, format!("  ok {remote}"));
                    pulled.push(remote.clone());
                }
                Err(err) => {
                    failures += 1;
                    record_file_failed(state, 1);
                    record_run_issue(
                        state,
                        "file_failed",
                        &label,
                        remote,
                        format!("tar dereference failed: {err:#}"),
                    );
                    log(
                        state,
                        format!("  FAILED {remote}: tar dereference failed: {err:#}"),
                    );
                }
            }
        }
    }

    if !dirs.is_empty() {
        set_current_file(
            state,
            Some(&format!("{} folder(s) from {label}", dirs.len())),
        );
        for remote in &dirs {
            set_current_file(state, Some(remote));
            match pull_paths_tar(host, &conn, std::slice::from_ref(remote), &excludes, state) {
                Ok(()) => {
                    record_file_ok(state);
                    log(state, format!("  ok {remote}"));
                    pulled.push(remote.clone());
                }
                Err(err) => {
                    failures += 1;
                    record_file_failed(state, 1);
                    record_run_issue(
                        state,
                        "dir_failed",
                        &label,
                        remote,
                        format!("tar failed: {err:#}"),
                    );
                    log(state, format!("  FAILED {remote}: tar failed: {err:#}"));
                }
            }
        }
    }

    for remote in &files {
        set_current_file(state, Some(remote));
        match pull_path(host, &conn, remote, &excludes, state) {
            Ok(()) => {
                record_file_ok(state);
                log(state, format!("  ok {remote}"));
                pulled.push(remote.to_string());
            }
            Err(err) => {
                failures += 1;
                record_file_failed(state, 1);
                record_run_issue(state, "file_failed", &label, remote, format!("{err:#}"));
                log(state, format!("  FAILED {remote}: {err:#}"));
            }
        }
    }
    // Windows can't preserve Unix modes on the pulled files, so capture them
    // from the remote into a per-host cache that `deploy` replays.
    if !pulled.is_empty() {
        if let Err(err) = record_host_perms(host, &conn, &pulled, &excludes, state) {
            log(state, format!("  perms: could not record: {err:#}"));
        }
    }
    failures
}

enum RemotePathKind {
    File,
    Dir,
    SymlinkFile,
}

fn remote_path_kind(conn: &SshConn, remote: &str) -> Result<RemotePathKind> {
    let q = shell_quote(remote);
    let out = ssh_capture(
        conn,
        &format!(
            "if [ -L {q} ]; then \
if [ -d {q} ]; then echo dir; elif [ -f {q} ]; then echo symlink_file; else echo broken_symlink; fi; \
elif [ -d {q} ]; then echo dir; elif [ -f {q} ]; then echo file; else echo missing; fi"
        ),
    )?;
    match out.trim() {
        "dir" => Ok(RemotePathKind::Dir),
        "file" => Ok(RemotePathKind::File),
        "symlink_file" => Ok(RemotePathKind::SymlinkFile),
        other => bail!("remote path is not a file or directory ({other})"),
    }
}

fn pull_paths_tar(
    host: &CollectorHost,
    conn: &SshConn,
    paths: &[String],
    excludes: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    fs::create_dir_all(&host.root).with_context(|| format!("creating {}", host.root.display()))?;
    let rel_paths: Vec<String> = paths
        .iter()
        .map(|path| remote_tar_path(path))
        .collect::<Result<_>>()?;
    for rel in &rel_paths {
        let local_path = host.root.join(local_rel_path(rel));
        clear_readonly_recursive(&local_path);
        remove_symlinks_recursive(&local_path);
    }

    let sqlite_snapshots = prepare_sqlite_snapshots(conn, &rel_paths, state)?;
    let mut snapshot_excludes: Vec<String> = sqlite_snapshots
        .iter()
        .map(|snapshot| snapshot.source_rel.clone())
        .collect();
    let mut exclude_patterns = tar_exclude_patterns(&rel_paths, excludes);
    exclude_patterns.append(&mut snapshot_excludes);
    let mut remote_cmd = String::from("cd / && tar -chf - ");
    if !exclude_patterns.is_empty() {
        remote_cmd.push_str("-X - ");
    }
    for rel in &rel_paths {
        remote_cmd.push_str(&shell_quote(rel));
        remote_cmd.push(' ');
    }
    log(
        state,
        format!(
            "  tar {} path(s) from {} -> {}",
            rel_paths.len(),
            conn.dest(),
            host.root.display()
        ),
    );

    let mut ssh = ssh_command(conn);
    ssh.arg(conn.dest()).arg(remote_cmd);
    ssh.stdout(Stdio::piped()).stderr(Stdio::piped());
    if exclude_patterns.is_empty() {
        ssh.stdin(Stdio::null());
    } else {
        ssh.stdin(Stdio::piped());
    }
    let mut ssh_child = ssh.spawn().context("running ssh tar")?;
    if !exclude_patterns.is_empty() {
        if let Some(mut stdin) = ssh_child.stdin.take() {
            stdin
                .write_all(exclude_patterns.join("\n").as_bytes())
                .context("writing tar exclude patterns")?;
            stdin.write_all(b"\n").ok();
        }
    }
    let ssh_stdout = ssh_child.stdout.take().context("opening ssh tar stdout")?;
    let extract_result = extract_tar_preserving_linux_names(ssh_stdout, &host.root, &rel_paths)
        .context("extracting tar stream")?;
    let ssh_output = ssh_child
        .wait_with_output()
        .context("waiting for ssh tar")?;
    let result = if !ssh_output.status.success() {
        Err(anyhow::anyhow!(
            "remote tar failed: {}",
            String::from_utf8_lossy(&ssh_output.stderr).trim()
        ))
    } else {
        for snapshot in &sqlite_snapshots {
            pull_sqlite_snapshot(host, conn, snapshot, state)?;
        }
        if extract_result.encoded_entries > 0 {
            log(
                state,
                format!(
                    "  name manifest: encoded {} Linux byte-name path(s)",
                    extract_result.encoded_entries
                ),
            );
        }
        Ok(())
    };
    cleanup_sqlite_snapshots(conn, &sqlite_snapshots, state);
    result
}

#[derive(Default)]
struct TarExtractResult {
    encoded_entries: usize,
}

#[derive(Default, Serialize, Deserialize)]
struct NameManifest {
    version: u32,
    entries: Vec<NameManifestEntry>,
}

#[derive(Serialize, Deserialize)]
struct NameManifestEntry {
    stored_rel: String,
    original_rel_hex: String,
}

fn extract_tar_preserving_linux_names<R: Read>(
    reader: R,
    root: &Path,
    rel_roots: &[String],
) -> Result<TarExtractResult> {
    let mut archive = tar::Archive::new(reader);
    let mut manifest = load_name_manifest(root).unwrap_or_default();
    manifest.version = 1;
    prune_name_manifest_roots(&mut manifest, rel_roots);
    let mut result = TarExtractResult::default();

    for entry in archive.entries().context("reading tar entries")? {
        let mut entry = entry.context("reading tar entry")?;
        let raw_path = entry.path_bytes().into_owned();
        let Some(stored_rel) = stored_rel_for_linux_path(&raw_path, &mut manifest)? else {
            continue;
        };
        let target = root.join(local_rel_path(&stored_rel));
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("creating directory {}", target.display()))?;
        } else if entry_type.is_symlink() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {}", parent.display()))?;
            }
            File::create(&target)
                .with_context(|| format!("creating symlink placeholder {}", target.display()))?;
        } else if entry_type.is_file() {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating directory {}", parent.display()))?;
            }
            let mut file =
                File::create(&target).with_context(|| format!("creating {}", target.display()))?;
            io::copy(&mut entry, &mut file)
                .with_context(|| format!("extracting {}", target.display()))?;
        }
    }

    result.encoded_entries = manifest.entries.len();
    write_name_manifest(root, manifest)?;
    Ok(result)
}

fn stored_rel_for_linux_path(
    raw_path: &[u8],
    manifest: &mut NameManifest,
) -> Result<Option<String>> {
    let mut stored_parts = Vec::new();
    let mut original_parts: Vec<Vec<u8>> = Vec::new();
    for part in raw_path.split(|b| *b == b'/') {
        if part.is_empty() || part == b"." {
            continue;
        }
        if part == b".." {
            bail!("tar entry contains '..'");
        }
        original_parts.push(part.to_vec());
        if let Some(text) = safe_windows_component(part) {
            stored_parts.push(text);
            continue;
        }

        let hash = blake3::hash(part).to_hex().to_string();
        stored_parts.push(ENCODED_NAME_DIR.to_string());
        stored_parts.push(hash[..32].to_string());
        let stored_rel = stored_parts.join("/");
        let original_rel_hex = hex_encode(&join_raw_path(&original_parts));
        if !manifest
            .entries
            .iter()
            .any(|entry| entry.stored_rel == stored_rel)
        {
            manifest.entries.push(NameManifestEntry {
                stored_rel,
                original_rel_hex,
            });
        }
    }
    if stored_parts.is_empty() {
        return Ok(None);
    }
    Ok(Some(stored_parts.join("/")))
}

fn safe_windows_component(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    if text.is_empty() || text == "." || text == ".." {
        return None;
    }
    if text.ends_with(' ') || text.ends_with('.') {
        return None;
    }
    if text
        .chars()
        .any(|ch| ch < ' ' || matches!(ch, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
    {
        return None;
    }
    let upper = text.split('.').next().unwrap_or(text).to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return None;
    }
    Some(text.to_string())
}

fn join_raw_path(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    out
}

fn write_name_manifest(root: &Path, manifest: NameManifest) -> Result<()> {
    let path = root.join(NAME_MANIFEST_FILE);
    if manifest.entries.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let body = serde_json::to_string_pretty(&manifest)?;
    fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

fn load_name_manifest(root: &Path) -> Result<NameManifest> {
    let path = root.join(NAME_MANIFEST_FILE);
    let body = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parsing {}", path.display()))
}

fn prune_name_manifest_roots(manifest: &mut NameManifest, rel_roots: &[String]) {
    manifest.entries.retain(|entry| {
        let stored_matches = rel_roots
            .iter()
            .any(|root| path_is_under_root(&entry.stored_rel, root));
        let original_matches = hex_decode(&entry.original_rel_hex)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .is_some_and(|original| {
                rel_roots
                    .iter()
                    .any(|root| path_is_under_root(&original, root))
            });
        !(stored_matches || original_matches)
    });
}

fn path_is_under_root(path: &str, root: &str) -> bool {
    path == root || path.starts_with(&format!("{root}/"))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        bail!("hex string has odd length");
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for idx in (0..bytes.len()).step_by(2) {
        let hi = hex_nibble(bytes[idx])?;
        let lo = hex_nibble(bytes[idx + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => bail!("invalid hex digit"),
    }
}

#[derive(Debug)]
struct SqliteSnapshot {
    source_rel: String,
    remote_tmp: String,
}

fn prepare_sqlite_snapshots(
    conn: &SshConn,
    rel_paths: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<Vec<SqliteSnapshot>> {
    let dir_roots: Vec<&String> = rel_paths
        .iter()
        .filter(|rel| {
            let q = shell_quote(rel);
            ssh_capture(conn, &format!("cd / && [ -d {q} ] && echo yes || true"))
                .map(|out| out.trim() == "yes")
                .unwrap_or(false)
        })
        .collect();
    if dir_roots.is_empty() {
        return Ok(Vec::new());
    }

    let mut find_commands = String::new();
    for root in dir_roots {
        find_commands.push_str("if [ -d ");
        find_commands.push_str(&shell_quote(root));
        find_commands.push_str(" ]; then find ");
        find_commands.push_str(&shell_quote(root));
        find_commands.push_str(" -type f -path '*/data/auto_sync.sqlite' -print; fi\n");
    }

    let script = format!(
        r#"
set -e
work="${{TMPDIR:-/tmp}}/auto_sync_collector_sqlite_snap.$$"
mkdir -p "$work"
cd /
{{
{find_commands}
}} | sort -u | while IFS= read -r db; do
    [ -n "$db" ] || continue
    hash="$(printf '%s' "$db" | sha256sum | awk '{{print $1}}')"
    snap="$work/$hash.sqlite"
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$db" "$snap" <<'PY_SQLITE_BACKUP'
import sqlite3
import sys

src, dst = sys.argv[1], sys.argv[2]
source = sqlite3.connect(f"file:{{src}}?mode=ro", uri=True, timeout=30)
target = sqlite3.connect(dst)
try:
    with target:
        source.backup(target)
finally:
    target.close()
    source.close()
PY_SQLITE_BACKUP
    elif command -v sqlite3 >/dev/null 2>&1; then
        sqlite3 "$db" ".backup '$snap'"
    else
        cp -f -- "$db" "$snap"
    fi
    chmod 600 "$snap" 2>/dev/null || true
    printf 'SQLITE_SNAPSHOT %s %s\n' "$db" "$snap"
done
"#
    );
    let remote_cmd = format!("sh -c {}", shell_quote(&script));
    let out = ssh_capture(conn, &remote_cmd)?;
    let mut snapshots = Vec::new();
    for line in out.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SQLITE_SNAPSHOT ") {
            if let Some((source_rel, remote_tmp)) = rest.rsplit_once(' ') {
                let source_rel = source_rel.trim_start_matches('/').to_string();
                let remote_tmp = remote_tmp.to_string();
                log(
                    state,
                    format!("  sqlite snapshot /{source_rel} -> {remote_tmp}"),
                );
                snapshots.push(SqliteSnapshot {
                    source_rel,
                    remote_tmp,
                });
            }
        }
    }
    Ok(snapshots)
}

fn pull_sqlite_snapshot(
    host: &CollectorHost,
    conn: &SshConn,
    snapshot: &SqliteSnapshot,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let local_path = host.root.join(local_rel_path(&snapshot.source_rel));
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    clear_readonly_recursive(&local_path);
    remove_symlinks_recursive(&local_path);
    log(
        state,
        format!(
            "  sqlite snapshot {}:{} -> {}",
            conn.dest(),
            snapshot.remote_tmp,
            local_path.display()
        ),
    );
    let mut cmd = scp_command(conn);
    cmd.arg(format!("{}:{}", conn.dest(), snapshot.remote_tmp));
    cmd.arg(&local_path);
    let output = cmd.output().context("copying sqlite snapshot")?;
    if !output.status.success() {
        bail!(
            "sqlite snapshot scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn cleanup_sqlite_snapshots(
    conn: &SshConn,
    snapshots: &[SqliteSnapshot],
    state: &Arc<Mutex<CollectorRunState>>,
) {
    let mut dirs = BTreeSet::new();
    for snapshot in snapshots {
        if let Some((dir, _)) = snapshot.remote_tmp.rsplit_once('/') {
            dirs.insert(dir.to_string());
        }
    }
    if dirs.is_empty() {
        return;
    }
    let mut cmd = String::from("rm -rf -- ");
    for dir in &dirs {
        cmd.push_str(&shell_quote(dir));
        cmd.push(' ');
    }
    match ssh_capture(conn, &cmd) {
        Ok(_) => log(
            state,
            "  sqlite snapshot cleanup: removed remote temp files",
        ),
        Err(err) => log(
            state,
            format!("  sqlite snapshot cleanup: could not remove remote temp files: {err:#}"),
        ),
    }
}

/// Normalize exclude entries to absolute paths without a trailing slash.
fn normalize_excludes(raw: &[String]) -> Vec<String> {
    raw.iter()
        .map(|e| e.trim().trim_end_matches('/').to_string())
        .filter(|e| !e.is_empty())
        .collect()
}

/// True if `path` is equal to, or nested under, any exclude entry.
fn is_excluded(path: &str, excludes: &[String]) -> bool {
    let path = path.trim_end_matches('/');
    excludes
        .iter()
        .any(|e| path == e || path.starts_with(&format!("{e}/")))
}

/// True if any exclude entry lives strictly *under* `dir` (so `dir` must be
/// pulled selectively rather than as one `scp -r`).
fn has_exclude_under(dir: &str, excludes: &[String]) -> bool {
    let prefix = format!("{}/", dir.trim_end_matches('/'));
    excludes.iter().any(|e| e.starts_with(&prefix))
}

/// Capture the Unix mode of every file/dir and every symlink target under each
/// pulled path. The per-host cache `<root>/.auto_sync_perms` uses compatible
/// `mode path` lines plus `symlink <base64-target> path` lines. `deploy`
/// replays these because Windows can't hold Unix modes or Linux symlinks on the
/// local copies.
/// Prefers `stat -c '%a %n'` (gives the octal mode directly); falls back to
/// parsing `ls -ldn` on hosts where `stat` is not installed yet.
fn record_host_perms(
    host: &CollectorHost,
    conn: &SshConn,
    paths: &[String],
    excludes: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    for path in paths {
        if is_excluded(path, excludes) {
            continue;
        }
        let q = shell_quote(path);
        let prune = find_prune_clause(path, excludes);
        let cmd = format!(
            "if command -v stat >/dev/null 2>&1; then \
find -H {q} {prune}\\( -type f -o -type d \\) -exec stat -c '%a %n' {{}} \\; 2>/dev/null; \
else \
find -H {q} {prune}\\( -type f -o -type d \\) -exec ls -ldn {{}} \\; 2>/dev/null; \
fi; \
if [ -L {q} ]; then \
t=$(readlink {q}) && b=$(printf %s \"$t\" | base64 | tr -d \"\\n\") && printf \"symlink %s %s\\n\" \"$b\" {q}; \
fi; \
find -H {q} -mindepth 1 {prune}-type l -exec sh -c 'for p do t=$(readlink \"$p\") || continue; b=$(printf %s \"$t\" | base64 | tr -d \"\\n\"); printf \"symlink %s %s\\n\" \"$b\" \"$p\"; done' sh {{}} + 2>/dev/null"
        );
        let out = match ssh_capture(conn, &cmd) {
            Ok(out) => out,
            Err(err) => {
                log(state, format!("  perms: {path}: {err:#}"));
                continue;
            }
        };
        for line in out.lines() {
            if let Some(entry_path) = perm_record_path(line) {
                if is_excluded(&entry_path, excludes) {
                    continue;
                }
                lines.push(line.trim().to_string());
            }
        }
    }
    lines.sort();
    lines.dedup();
    let file = host.root.join(PERMS_FILE);
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(&file, body).with_context(|| format!("writing {}", file.display()))?;
    log(state, format!("  perms: recorded {} entries", lines.len()));
    Ok(())
}

fn find_prune_clause(root: &str, excludes: &[String]) -> String {
    let root = root.trim_end_matches('/');
    let prefix = format!("{root}/");
    let mut tests = Vec::new();
    for exclude in excludes {
        let exclude = exclude.trim_end_matches('/');
        if exclude == root || !exclude.starts_with(&prefix) {
            continue;
        }
        tests.push(format!("-path {}", shell_quote(exclude)));
        tests.push(format!("-path {}", shell_quote(&format!("{exclude}/*"))));
    }
    if tests.is_empty() {
        String::new()
    } else {
        format!("\\( {} \\) -prune -o ", tests.join(" -o "))
    }
}

fn perm_record_path(line: &str) -> Option<String> {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("symlink ") {
        let (_, path) = rest.split_once(char::is_whitespace)?;
        let path = path.trim();
        if path.starts_with('/') {
            return Some(path.to_string());
        }
        return None;
    }
    parse_perm_line(line).map(|(_, path)| path)
}

/// Parse one permission line into (octal_mode, absolute_path). Accepts both
/// `stat -c '%a %n'` output (`755 /path` — mode already octal) and `ls -ldn`
/// output (`drwxr-xr-x … /path` — symbolic, may carry setuid/setgid/sticky).
fn parse_perm_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    // stat format: an octal mode token, then the path (rest of the line).
    if let Some((first, rest)) = line.split_once(char::is_whitespace) {
        let rest = rest.trim();
        if !first.is_empty()
            && first.len() <= 5
            && first.bytes().all(|b| b.is_ascii_digit())
            && rest.starts_with('/')
        {
            return Some((first.to_string(), rest.to_string()));
        }
    }
    // ls -ldn fallback: symbolic perms first, absolute path last.
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 9 {
        return None;
    }
    let sym = parts[0].as_bytes();
    if sym.len() < 10 {
        return None;
    }
    let path = *parts.last()?;
    if !path.starts_with('/') {
        return None;
    }
    let p = &sym[1..10];
    let mut mode = 0u32;
    if p[0] == b'r' {
        mode |= 0o400;
    }
    if p[1] == b'w' {
        mode |= 0o200;
    }
    match p[2] {
        b'x' => mode |= 0o100,
        b's' => mode |= 0o4100,
        b'S' => mode |= 0o4000,
        _ => {}
    }
    if p[3] == b'r' {
        mode |= 0o40;
    }
    if p[4] == b'w' {
        mode |= 0o20;
    }
    match p[5] {
        b'x' => mode |= 0o10,
        b's' => mode |= 0o2010,
        b'S' => mode |= 0o2000,
        _ => {}
    }
    if p[6] == b'r' {
        mode |= 0o4;
    }
    if p[7] == b'w' {
        mode |= 0o2;
    }
    match p[8] {
        b'x' => mode |= 0o1,
        b't' => mode |= 0o1001,
        b'T' => mode |= 0o1000,
        _ => {}
    }
    Some((format!("{mode:o}"), path.to_string()))
}

/// Pull one remote path, reconstructing its directory structure under the
/// host's local root: root `D:\share\linux\aws` + remote `/usr/local/x` gives
/// `D:\share\linux\aws\usr\local\x`.
fn pull_path(
    host: &CollectorHost,
    conn: &SshConn,
    remote: &str,
    excludes: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let parent_components = remote_parent_components(remote)?;
    let mut local_parent = host.root.clone();
    for component in &parent_components {
        local_parent.push(component);
    }
    fs::create_dir_all(&local_parent)
        .with_context(|| format!("creating {}", local_parent.display()))?;

    // If an excluded path lives inside this directory, walk it and copy only the
    // children that are not (or do not contain) excludes — so excluded subtrees
    // are never transferred.
    if has_exclude_under(remote, excludes) {
        let leaf = remote.rsplit('/').next().unwrap_or(remote);
        let local_dir = local_parent.join(leaf);
        return pull_dir_excluding(conn, remote, &local_dir, excludes, state);
    }

    log(
        state,
        format!(
            "  scp {}:{} -> {}",
            conn.dest(),
            remote,
            local_parent.display()
        ),
    );
    scp_recursive(conn, remote, &local_parent)
}

/// Copy `remote` (a file or whole directory subtree) into `local_dest` with
/// `scp -r`. `local_dest` is the parent that will contain the copied leaf.
///
/// We deliberately do NOT pass `-p`: preserving the source mode stamped git's
/// read-only 0444 objects onto the local copy, which the next pull could not
/// overwrite (`open local … Permission denied`). The Unix modes are captured
/// separately in the per-host `.auto_sync_perms` cache and restored on deploy,
/// so `-p` is unnecessary here. Any legacy read-only copies from earlier `-p`
/// pulls are cleared first so they can still be overwritten.
fn scp_recursive(conn: &SshConn, remote: &str, local_dest: &Path) -> Result<()> {
    if let Some(leaf) = remote.trim_end_matches('/').rsplit('/').next() {
        let local_path = local_dest.join(leaf);
        clear_readonly_recursive(&local_path);
        remove_symlinks_recursive(&local_path);
    }

    let mut cmd = scp_command(conn);
    cmd.arg("-r");
    cmd.arg(format!("{}:{}", conn.dest(), remote));
    cmd.arg(local_dest);
    let output = cmd.output().context("running scp")?;
    if !output.status.success() {
        bail!(
            "scp failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Recursively clear the read-only attribute on `path` (a file or directory) so
/// a subsequent overwrite/scp can replace it. Best-effort: errors are ignored.
fn clear_readonly_recursive(path: &Path) {
    if !path.exists() {
        return;
    }
    for entry in WalkDir::new(path).into_iter().flatten() {
        if let Ok(meta) = entry.metadata() {
            let mut perms = meta.permissions();
            if perms.readonly() {
                perms.set_readonly(false);
                let _ = fs::set_permissions(entry.path(), perms);
            }
        }
    }
}

/// Remove symlinks/junctions under `path` before copying real data over them.
/// This keeps extraction/copy from writing through an old local link target.
fn remove_symlinks_recursive(path: &Path) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        let _ = remove_symlink_path(path);
        return;
    }
    if !meta.is_dir() {
        return;
    }
    let mut links: Vec<PathBuf> = WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_symlink())
        .map(|entry| entry.path().to_path_buf())
        .collect();
    links.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for link in links {
        let _ = remove_symlink_path(&link);
    }
}

fn remove_symlink_path(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false) {
            return fs::remove_dir(path);
        }
        return fs::remove_file(path);
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(path)
    }
}

/// Pull `remote_dir` into `local_dir`, honoring `excludes`: skip excluded
/// children outright, recurse into children that themselves contain an exclude,
/// and `scp -r` the rest wholesale.
fn pull_dir_excluding(
    conn: &SshConn,
    remote_dir: &str,
    local_dir: &Path,
    excludes: &[String],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    fs::create_dir_all(local_dir).with_context(|| format!("creating {}", local_dir.display()))?;
    let names = list_remote_names(conn, remote_dir)?;
    for name in names {
        let child = join_remote(remote_dir, &name);
        if is_excluded(&child, excludes) {
            log(state, format!("  ignore {child} (excluded)"));
            continue;
        }
        set_current_file(state, Some(&child));
        if has_exclude_under(&child, excludes) {
            pull_dir_excluding(conn, &child, &local_dir.join(&name), excludes, state)?;
        } else {
            scp_recursive(conn, &child, local_dir)?;
        }
    }
    Ok(())
}

/// List the immediate entry names (files and dirs, including dotfiles) of a
/// remote directory. Uses `ls -A1` piped through a read loop so an unmatched
/// glob never aborts under zsh.
fn list_remote_names(conn: &SshConn, dir: &str) -> Result<Vec<String>> {
    let cmd = format!(
        "cd -- {} 2>/dev/null && ls -A1 2>/dev/null",
        shell_quote(dir)
    );
    let out = ssh_capture(conn, &cmd)?;
    Ok(out
        .lines()
        .map(|l| l.trim_end_matches('\r').to_string())
        .filter(|l| !l.is_empty() && l != "." && l != "..")
        .collect())
}

// --- deploy (run the host's deploy script on Windows) -------------------------

/// Deploy a single host by running its deploy script **on this machine**
/// (Windows), not on the remote host. The script drives the bundled `ssh`/`scp`
/// itself — it decides what to push, how to fix permissions, how to restart
/// services. The engine only hands it a ready-made environment:
///   AS_SSH / AS_SCP        the ssh/scp binaries to use (bundled, else PATH)
///   AS_HOSTNAME / AS_USER / AS_DEST / AS_PORT / AS_KEY   this host's connection
///   AS_ROOT                the host's local collected root (e.g. D:\share\openwrt)
///   AS_HOST_<NAME>         every collector host's hostname (so a script can, e.g.,
///                          read AS_HOST_AWS to substitute a server address)
pub fn deploy(
    host: CollectorHost,
    all_hosts: Vec<CollectorHost>,
    state: Arc<Mutex<CollectorRunState>>,
) {
    let ok = match deploy_inner(&host, &all_hosts, &state) {
        Ok(0) => {
            log(&state, "Deploy finished.");
            true
        }
        Ok(n) => {
            log(&state, format!("Deploy finished with {n} error(s)."));
            false
        }
        Err(err) => {
            log(&state, format!("ERROR: {err:#}"));
            false
        }
    };
    finish_state(&state, ok);
}

fn deploy_inner(
    host: &CollectorHost,
    all_hosts: &[CollectorHost],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<usize> {
    if host.hostname.trim().is_empty() {
        bail!("host has no hostname");
    }
    if host.root.as_os_str().is_empty() {
        bail!("host has no local root");
    }
    let conn = SshConn::from_host(host);
    let label = if host.name.trim().is_empty() {
        host.hostname.clone()
    } else {
        host.name.clone()
    };
    log(
        state,
        format!(
            "=== deploy {label} ({}) — running on this machine ===",
            conn.dest()
        ),
    );

    if host.deploy_script_path.trim().is_empty() && host.deploy_script.trim().is_empty() {
        log(state, "no deploy script for this host — nothing to do");
        return Ok(0);
    }

    match run_local_deploy_script(host, &conn, all_hosts, state) {
        Ok(true) => {
            log(state, "deploy script ok");
            Ok(0)
        }
        Ok(false) => {
            log(state, "deploy script exited non-zero");
            Ok(1)
        }
        Err(err) => {
            log(state, format!("deploy script error: {err:#}"));
            Ok(1)
        }
    }
}

/// True if `s` looks like a dotted IPv4 literal (four 0-255 octets).
fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.trim().split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().map(|n| n <= 255).unwrap_or(false)
        })
}

/// Rewrite a shadowsocks-rust client config so the `server` field of the entry
/// matching `new_server`'s address family is set to `new_server`: an IPv4
/// hostname updates the IPv4 server entry, an IPv6 hostname the IPv6 one. The
/// file is parsed as JSON and re-serialized, so no brittle text munging is
/// involved. Returns the new JSON text; errs if the family has no server entry.
fn substitute_ss_server(conf_text: &str, new_server: &str) -> Result<String> {
    let new_server = new_server.trim();
    let want_v4 = is_ipv4(new_server);
    let want_v6 = !want_v4 && new_server.contains(':');
    if !want_v4 && !want_v6 {
        bail!("aws hostname '{new_server}' is neither IPv4 nor IPv6; not substituting");
    }
    let mut root: serde_json::Value =
        serde_json::from_str(conf_text).context("parsing shadowsocks client json")?;
    let mut changed = 0usize;
    if let Some(servers) = root.get_mut("servers").and_then(|v| v.as_array_mut()) {
        for entry in servers.iter_mut() {
            let Some(cur) = entry.get("server").and_then(|v| v.as_str()) else {
                continue;
            };
            let entry_v4 = is_ipv4(cur);
            let entry_v6 = !entry_v4 && cur.contains(':');
            let matches = (want_v4 && entry_v4) || (want_v6 && entry_v6);
            if matches && cur != new_server {
                entry["server"] = serde_json::Value::String(new_server.to_string());
                changed += 1;
            } else if matches {
                changed += 1; // already correct, but the family entry exists
            }
        }
    }
    if changed == 0 {
        bail!(
            "no {} server entry found in the shadowsocks config",
            if want_v4 { "IPv4" } else { "IPv6" }
        );
    }
    serde_json::to_string_pretty(&root).context("serializing shadowsocks client json")
}

/// Prefer a bundled OpenSSH binary shipped next to the running executable
/// (`<exe dir>\windows\openssh\OpenSSH-Win64\<tool>.exe`), falling back to the
/// system tool on PATH.
fn resolve_ssh_tool(tool: &str) -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let exe_name = format!("{tool}.exe");
            let candidates = [
                dir.join("windows")
                    .join("openssh")
                    .join("OpenSSH-Win64")
                    .join(&exe_name),
                dir.join("openssh").join("OpenSSH-Win64").join(&exe_name),
                dir.join(&exe_name),
            ];
            for cand in candidates {
                if cand.exists() {
                    return cand.to_string_lossy().into_owned();
                }
            }
        }
    }
    tool.to_string()
}

/// Turn a host name into an env-var-safe suffix: `AS_HOST_<UPPER>` where every
/// non-alphanumeric character becomes `_`.
fn env_host_suffix(name: &str) -> String {
    let mut s: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push('_');
    }
    s
}

/// Write the deploy script to a temp file and run it locally (PowerShell on
/// Windows), streaming its output into the run log.
fn run_local_deploy_script(
    host: &CollectorHost,
    conn: &SshConn,
    all_hosts: &[CollectorHost],
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<bool> {
    let file_tag = env_host_suffix(&host.name).to_lowercase();
    let mut temp_script_path: Option<PathBuf> = None;
    let script_path = if host.deploy_script_path.trim().is_empty() {
        let path = std::env::temp_dir().join(format!("auto_sync_deploy_{file_tag}.ps1"));
        fs::write(&path, &host.deploy_script)
            .with_context(|| format!("writing {}", path.display()))?;
        temp_script_path = Some(path.clone());
        path
    } else {
        let path = PathBuf::from(host.deploy_script_path.trim());
        if !path.exists() {
            bail!("deploy script file not found: {}", path.display());
        }
        path
    };

    #[cfg(windows)]
    let mut cmd = {
        let mut c = command("powershell");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = command("pwsh");
        c.args(["-NoProfile", "-NonInteractive", "-File"]);
        c
    };
    cmd.arg(&script_path);

    cmd.env("AS_SSH", resolve_ssh_tool("ssh"));
    cmd.env("AS_SCP", resolve_ssh_tool("scp"));
    cmd.env("AS_HOSTNAME", conn.hostname);
    cmd.env("AS_USER", conn.user);
    cmd.env("AS_DEST", conn.dest());
    cmd.env(
        "AS_PORT",
        if conn.port == 0 {
            String::new()
        } else {
            conn.port.to_string()
        },
    );
    cmd.env(
        "AS_KEY",
        if conn.identity_file.is_empty() {
            String::new()
        } else {
            expand_tilde(conn.identity_file)
        },
    );
    if !host.password.trim().is_empty() {
        cmd.env("AS_PASSWORD", host.password.trim());
    }
    cmd.env("AS_ROOT", host.root.as_os_str());
    cmd.env("AS_COLLECT_PATHS", host.paths.join("\n"));
    cmd.env("AS_EXCLUDE_PATHS", host.exclude.join("\n"));
    for other in all_hosts {
        cmd.env(
            format!("AS_HOST_{}", env_host_suffix(&other.name)),
            other.hostname.trim(),
        );
    }

    // Hand the script the per-host permission cache captured at collect time so
    // it can restore modes Windows dropped.
    let perms_file = host.root.join(PERMS_FILE);
    if perms_file.exists() {
        cmd.env("AS_PERMS_FILE", perms_file.as_os_str());
    }

    // If this host collected a shadowsocks client config, rewrite its server
    // address (family-matched to the `aws` host's hostname) here in Rust and
    // hand the script the substituted file via AS_SS_CLIENT_CONF — the JSON
    // rewrite is far safer done here than with string munging in the script.
    let ss_conf_temp = prepare_ss_client_conf(host, all_hosts, &file_tag, state);
    if let Some(ref temp) = ss_conf_temp {
        cmd.env("AS_SS_CLIENT_CONF", temp.as_os_str());
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let output = cmd.output().context("running deploy script (powershell)")?;
    for line in String::from_utf8_lossy(&output.stdout).lines().take(400) {
        log(state, format!("  | {line}"));
    }
    for line in String::from_utf8_lossy(&output.stderr).lines().take(200) {
        log(state, format!("  ! {line}"));
    }
    if let Some(path) = temp_script_path {
        let _ = fs::remove_file(path);
    }
    if let Some(temp) = ss_conf_temp {
        let _ = fs::remove_file(&temp);
    }
    Ok(output.status.success())
}

/// If `host` collected `usr/local/shadowsocks/conf/shadowsocks-client.json` and
/// a host named `aws` exists, write a server-substituted copy to a temp file and
/// return its path (for the deploy script to scp up). Returns None (and logs)
/// when nothing needs doing; a substitution error is logged and yields None.
fn prepare_ss_client_conf(
    host: &CollectorHost,
    all_hosts: &[CollectorHost],
    file_tag: &str,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Option<PathBuf> {
    let conf_local = host
        .root
        .join("usr")
        .join("local")
        .join("shadowsocks")
        .join("conf")
        .join("shadowsocks-client.json");
    if !conf_local.exists() {
        return None;
    }
    let Some(aws) = all_hosts
        .iter()
        .find(|h| h.name.trim().eq_ignore_ascii_case("aws"))
        .map(|h| h.hostname.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        log(
            state,
            "shadowsocks conf present but no 'aws' host — leaving server address unchanged",
        );
        return None;
    };
    let text = match fs::read_to_string(&conf_local) {
        Ok(text) => text,
        Err(err) => {
            log(
                state,
                format!("could not read {}: {err}", conf_local.display()),
            );
            return None;
        }
    };
    match substitute_ss_server(&text, &aws) {
        Ok(new_text) => {
            let temp = std::env::temp_dir().join(format!("auto_sync_ss_client_{file_tag}.json"));
            if let Err(err) = fs::write(&temp, new_text) {
                log(state, format!("could not write substituted conf: {err}"));
                return None;
            }
            let family = if is_ipv4(&aws) { "IPv4" } else { "IPv6" };
            log(
                state,
                format!("shadowsocks server address -> {aws} ({family})"),
            );
            Some(temp)
        }
        Err(err) => {
            log(
                state,
                format!("shadowsocks server substitution skipped: {err:#}"),
            );
            None
        }
    }
}

/// Track every file at or above `threshold_mb` MiB under `git_dir` with Git LFS.
/// Also removes stale legacy `.autosplit.*` parts from older collector runs.
fn track_large_files_with_lfs(
    git_dir: &Path,
    threshold_mb: u64,
    state: &Arc<Mutex<CollectorRunState>>,
) -> Result<()> {
    let threshold = threshold_mb.saturating_mul(1024 * 1024);
    if threshold == 0 {
        return Ok(());
    }
    run_git(git_dir, &["lfs", "install", "--local"], state)
        .context("initializing git lfs for collector repository")?;

    let mut ignore = GitignoreEditor::load(git_dir)?;
    let mut large_files = Vec::new();
    for entry in WalkDir::new(git_dir)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default();
        if name.contains(AUTOSPLIT_MARKER) {
            let _ = fs::remove_file(path);
            log(
                state,
                format!("  removed legacy split part {}", path.display()),
            );
            continue;
        }
        let len = match entry.metadata() {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        if len <= threshold {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(git_dir) {
            remove_existing_parts(path)?;
            ignore.remove(rel);
            let rel_text = rel.to_string_lossy().replace('\\', "/");
            run_git(git_dir, &["lfs", "track", "--", &rel_text], state)
                .with_context(|| format!("git lfs track {rel_text}"))?;
            large_files.push(rel_text.clone());
            log(
                state,
                format!("  lfs track {rel_text} ({} MiB)", len / 1024 / 1024),
            );
        }
    }
    ignore.save()?;
    if !large_files.is_empty() {
        for rel in &large_files {
            run_git(git_dir, &["add", "--renormalize", "--", rel], state)
                .with_context(|| format!("git add --renormalize {rel}"))?;
        }
    }
    Ok(())
}

/// Remove any `<name>.autosplit.*` siblings of `path` (stale parts).
fn remove_existing_parts(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(file_name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}{AUTOSPLIT_MARKER}");
    if let Ok(dir) = fs::read_dir(parent) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&prefix) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// `git add -A`, commit (only if something changed), then best-effort push.
fn commit_and_push(git_dir: &Path, state: &Arc<Mutex<CollectorRunState>>) -> Result<()> {
    run_git(git_dir, &["add", "-A"], state)?;
    let status = git_capture(git_dir, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        log(state, "nothing to commit");
    } else {
        let message = format!(
            "collector: {}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        let mut args: Vec<String> = Vec::new();
        // Supply a fallback identity only if the repo/global config lacks one,
        // so an unconfigured git does not hard-fail the commit.
        if git_capture(git_dir, &["config", "user.email"])
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            args.push("-c".into());
            args.push("user.email=collector@auto_sync".into());
            args.push("-c".into());
            args.push("user.name=auto_sync collector".into());
        }
        args.push("commit".into());
        args.push("-m".into());
        args.push(message.clone());
        let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        run_git(git_dir, &refs, state)?;
        log(state, format!("committed: {message}"));
    }

    let (ok, output) = git_status(git_dir, &["push"])?;
    for line in output.lines().take(20) {
        log(state, format!("  git push | {line}"));
    }
    if ok {
        log(state, "pushed");
    } else {
        log(state, "git push returned non-zero (check remote/upstream)");
    }
    Ok(())
}

/// Browse a remote directory over ssh using the request's connection
/// parameters.
pub fn browse(req: &CollectorBrowseRequest) -> Result<CollectorBrowseResponse> {
    let conn = SshConn::from_request(req);
    if conn.hostname.is_empty() {
        bail!("no hostname to connect to");
    }
    let path = {
        let trimmed = req.path.trim();
        if trimmed.is_empty() { "/" } else { trimmed }
    };
    // List entries and mark directories with a trailing `/`, using a `[ -d ]`
    // test (which follows symlinks, so a symlink-to-dir is flagged correctly).
    // We iterate `ls -A1` output rather than a shell glob because an unmatched
    // glob is a fatal error under zsh (`no matches found`) whereas it stays
    // literal in sh — the `ls | while read` form behaves the same in zsh, bash,
    // ash and busybox.
    let remote_cmd = format!(
        "cd -- {} 2>/dev/null || exit 0; \
ls -A1 2>/dev/null | while IFS= read -r f; do \
[ -e \"$f\" ] || [ -L \"$f\" ] || continue; \
if [ -d \"$f\" ]; then printf '%s/\\n' \"$f\"; else printf '%s\\n' \"$f\"; fi; \
done",
        shell_quote(path)
    );
    let out = ssh_capture(&conn, &remote_cmd)?;
    let mut entries = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let is_dir = line.ends_with('/');
        let name = line.trim_end_matches('/').to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let child = join_remote(path, &name);
        entries.push(CollectorBrowseEntry {
            name,
            path: child,
            is_dir,
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.cmp(&b.name)));
    Ok(CollectorBrowseResponse {
        path: path.to_string(),
        parent: remote_parent(path),
        entries,
    })
}

// --- ssh / git command helpers ------------------------------------------------

/// Run a remote command over ssh, returning stdout (fails on non-zero exit).
fn ssh_capture(conn: &SshConn, remote_cmd: &str) -> Result<String> {
    let mut cmd = ssh_command(conn);
    cmd.arg(conn.dest()).arg(remote_cmd);
    let output = cmd.output().context("running ssh")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ssh failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(git_dir: &Path) -> Command {
    let mut cmd = command("git");
    cmd.current_dir(git_dir);
    cmd
}

/// Run a git subcommand, logging stderr; bail on non-zero exit.
fn run_git(git_dir: &Path, args: &[&str], state: &Arc<Mutex<CollectorRunState>>) -> Result<()> {
    let output = git(git_dir).args(args).output().context("running git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        for line in stderr.lines().take(20) {
            log(
                state,
                format!("  git {} | {line}", args.first().copied().unwrap_or("")),
            );
        }
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(())
}

/// Run a git subcommand, returning stdout (fails on non-zero exit).
fn git_capture(git_dir: &Path, args: &[&str]) -> Result<String> {
    let output = git(git_dir).args(args).output().context("running git")?;
    if !output.status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or(""));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a git subcommand, returning (success, stdout+stderr) — never bails.
fn git_status(git_dir: &Path, args: &[&str]) -> Result<(bool, String)> {
    let output = git(git_dir).args(args).output().context("running git")?;
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok((output.status.success(), combined))
}

// --- path helpers -------------------------------------------------------------

/// The parent path components of a remote absolute path, sanitized so nothing
/// can escape the local root. `/usr/local/x` -> ["usr", "local"].
fn remote_parent_components(remote: &str) -> Result<Vec<String>> {
    let trimmed = remote.trim().trim_end_matches('/');
    let mut components: Vec<String> = trimmed
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .map(|c| c.to_string())
        .collect();
    if components.is_empty() {
        bail!("refusing to pull the remote root '/'");
    }
    if components.iter().any(|c| c == "..") {
        bail!("remote path must not contain '..'");
    }
    components.pop(); // drop the leaf; scp recreates it under the parent
    Ok(components)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_tar_path(remote: &str) -> Result<String> {
    let trimmed = remote.trim().trim_end_matches('/');
    let parts: Vec<&str> = trimmed
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() {
        bail!("refusing to archive the remote root '/'");
    }
    if parts.iter().any(|part| *part == "..") {
        bail!("remote path must not contain '..'");
    }
    Ok(parts.join("/"))
}

fn local_rel_path(rel: &str) -> PathBuf {
    let mut path = PathBuf::new();
    for part in rel.split('/') {
        if !part.is_empty() {
            path.push(part);
        }
    }
    path
}

fn tar_exclude_patterns(roots: &[String], excludes: &[String]) -> Vec<String> {
    let mut patterns = Vec::new();
    for exclude in excludes {
        if let Ok(rel) = remote_tar_path(exclude) {
            if !roots
                .iter()
                .any(|root| rel == *root || rel.starts_with(&format!("{root}/")))
            {
                continue;
            }
            patterns.push(rel.clone());
            patterns.push(format!("{rel}/*"));
        }
    }
    patterns
}

fn join_remote(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", base.trim_end_matches('/'), name)
    }
}

fn remote_parent(path: &str) -> Option<String> {
    if path == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some("/".to_string()),
        Some((parent, _)) => Some(parent.to_string()),
        None => Some("/".to_string()),
    }
}

/// Reads, dedups, and rewrites `.gitignore` entries for split originals.
struct GitignoreEditor {
    path: PathBuf,
    lines: Vec<String>,
    dirty: bool,
}

impl GitignoreEditor {
    fn load(git_dir: &Path) -> Result<Self> {
        let path = git_dir.join(".gitignore");
        let lines = match fs::read_to_string(&path) {
            Ok(text) => text.lines().map(|l| l.to_string()).collect(),
            Err(_) => Vec::new(),
        };
        Ok(Self {
            path,
            lines,
            dirty: false,
        })
    }

    fn remove(&mut self, rel: &Path) {
        let mut entry = String::from("/");
        entry.push_str(&rel.to_string_lossy().replace('\\', "/"));
        let before = self.lines.len();
        self.lines.retain(|line| line.trim() != entry);
        if self.lines.len() != before {
            self.dirty = true;
        }
    }

    fn save(&self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut body = self.lines.join("\n");
        body.push('\n');
        fs::write(&self.path, body).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_parent_components_reconstructs_structure() {
        assert_eq!(
            remote_parent_components("/usr/local/shadowsocks").unwrap(),
            vec!["usr", "local"]
        );
        assert_eq!(
            remote_parent_components("/etc").unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(remote_parent_components("/a/b/c/").unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn remote_parent_components_rejects_escapes() {
        assert!(remote_parent_components("/").is_err());
        assert!(remote_parent_components("/a/../b").is_err());
    }

    #[test]
    fn remote_tar_path_sanitizes_archive_entries() {
        assert_eq!(
            remote_tar_path("/etc/config/dhcp").unwrap(),
            "etc/config/dhcp"
        );
        assert_eq!(remote_tar_path("/usr/local/bin/").unwrap(), "usr/local/bin");
        assert!(remote_tar_path("/").is_err());
        assert!(remote_tar_path("/a/../b").is_err());
        assert_eq!(
            local_rel_path("etc/config/dhcp"),
            PathBuf::from("etc").join("config").join("dhcp")
        );
    }

    #[test]
    fn tar_exclude_patterns_exclude_only_paths_under_archived_roots() {
        let patterns = tar_exclude_patterns(
            &["usr/local/blog".to_string()],
            &[
                "/usr/local/blog/logs/".to_string(),
                "/usr/local/blog/.backup-worktree".to_string(),
                "/root/.ssh".to_string(),
                "/usr/local/tbox/log".to_string(),
            ],
        );
        assert_eq!(
            patterns,
            vec![
                "usr/local/blog/logs",
                "usr/local/blog/logs/*",
                "usr/local/blog/.backup-worktree",
                "usr/local/blog/.backup-worktree/*",
            ]
        );
    }

    #[test]
    fn linux_name_manifest_keeps_safe_utf8_names_plain() {
        let mut manifest = NameManifest::default();
        let stored = stored_rel_for_linux_path(
            "root/.halo2/attachments/upload/微信图片.png".as_bytes(),
            &mut manifest,
        )
        .unwrap()
        .unwrap();
        assert_eq!(stored, "root/.halo2/attachments/upload/微信图片.png");
        assert!(manifest.entries.is_empty());
    }

    #[test]
    fn linux_name_manifest_encodes_windows_unsafe_components() {
        let mut manifest = NameManifest::default();
        let stored = stored_rel_for_linux_path(b"root/upload/bad:name.png", &mut manifest)
            .unwrap()
            .unwrap();
        assert!(stored.starts_with("root/upload/.auto_sync_name/"));
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(manifest.entries[0].stored_rel, stored);
        assert_eq!(
            hex_decode(&manifest.entries[0].original_rel_hex).unwrap(),
            b"root/upload/bad:name.png"
        );
    }

    #[test]
    fn linux_name_manifest_encodes_non_utf8_components() {
        let mut manifest = NameManifest::default();
        let stored = stored_rel_for_linux_path(b"root/upload/\xff.png", &mut manifest)
            .unwrap()
            .unwrap();
        assert!(stored.starts_with("root/upload/.auto_sync_name/"));
        assert_eq!(manifest.entries.len(), 1);
        assert_eq!(
            hex_decode(&manifest.entries[0].original_rel_hex).unwrap(),
            b"root/upload/\xff.png"
        );
    }

    #[test]
    fn remote_navigation_helpers() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/usr/local", "bin"), "/usr/local/bin");
        assert_eq!(remote_parent("/"), None);
        assert_eq!(remote_parent("/etc"), Some("/".to_string()));
        assert_eq!(
            remote_parent("/usr/local/bin"),
            Some("/usr/local".to_string())
        );
    }

    #[test]
    fn clear_readonly_makes_files_writable() {
        let dir = std::env::temp_dir().join(format!("collector_ro_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("obj");
        fs::write(&file, b"x").unwrap();
        let mut perms = fs::metadata(&file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&file, perms).unwrap();
        assert!(fs::metadata(&file).unwrap().permissions().readonly());
        clear_readonly_recursive(&dir);
        assert!(!fs::metadata(&file).unwrap().permissions().readonly());
        // overwriting now succeeds
        fs::write(&file, b"yy").unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_matching() {
        let ex = normalize_excludes(&[
            "/usr/local/blog/logs/".to_string(),
            "/usr/local/blog/.backup-worktree".to_string(),
        ]);
        assert!(is_excluded("/usr/local/blog/logs", &ex));
        assert!(is_excluded("/usr/local/blog/logs/app.log", &ex));
        assert!(is_excluded("/usr/local/blog/.backup-worktree", &ex));
        assert!(!is_excluded("/usr/local/blog", &ex));
        assert!(!is_excluded("/usr/local/blog/logsX", &ex)); // not a path boundary
        assert!(!is_excluded("/usr/local/blog/src", &ex));
        // the parent dir contains excludes -> must be pulled selectively
        assert!(has_exclude_under("/usr/local/blog", &ex));
        assert!(!has_exclude_under("/usr/local/tbox", &ex));
        assert!(!has_exclude_under("/usr/local/blog/logs", &ex));
    }

    #[test]
    fn parse_perm_line_handles_stat_and_ls() {
        // stat -c '%a %n' (mode already octal)
        assert_eq!(
            parse_perm_line("755 /usr/local/shadowsocks/bin"),
            Some(("755".to_string(), "/usr/local/shadowsocks/bin".to_string()))
        );
        assert_eq!(
            parse_perm_line("644 /etc/config/dhcp"),
            Some(("644".to_string(), "/etc/config/dhcp".to_string()))
        );
        assert_eq!(parse_perm_line("4755 /bin/su").unwrap().0, "4755");
        // ls -ldn fallback (symbolic)
        let dir =
            "drwxr-xr-x    2 0        0             4096 Jun 30 21:40 /usr/local/shadowsocks/bin";
        assert_eq!(
            parse_perm_line(dir),
            Some(("755".to_string(), "/usr/local/shadowsocks/bin".to_string()))
        );
        let key = "-rw-------    1 0 0 100 Jan 1 00:00 /root/.ssh/id_ed25519";
        assert_eq!(
            parse_perm_line(key),
            Some(("600".to_string(), "/root/.ssh/id_ed25519".to_string()))
        );
        let suid = "-rwsr-xr-x 1 0 0 1 Jan 1 00:00 /bin/su";
        assert_eq!(parse_perm_line(suid).unwrap().0, "4755");
        let sticky = "drwxrwxrwt 2 0 0 1 Jan 1 00:00 /tmp";
        assert_eq!(parse_perm_line(sticky).unwrap().0, "1777");
        assert_eq!(parse_perm_line("garbage"), None);
    }

    #[test]
    fn perm_record_path_handles_modes_and_symlinks() {
        assert_eq!(
            perm_record_path("755 /usr/local/bin/tool"),
            Some("/usr/local/bin/tool".to_string())
        );
        assert_eq!(
            perm_record_path("symlink Li4vc2hhcmVkL3RhcmdldA== /usr/local/bin/tool link"),
            Some("/usr/local/bin/tool link".to_string())
        );
        assert_eq!(
            perm_record_path("symlink Li4vc2hhcmVkL3RhcmdldA== relative"),
            None
        );
        assert_eq!(perm_record_path("symlink missing-path"), None);
    }

    #[test]
    fn ss_server_substitution_is_family_matched() {
        let conf = r#"{
  "servers": [
    { "server": "2406:da18:c4a:2281::1", "server_port": 443, "disabled": true },
    { "server": "54.179.191.126", "server_port": 443, "disabled": false }
  ]
}"#;
        // IPv4 aws -> only the IPv4 entry changes.
        let out = substitute_ss_server(conf, "203.0.113.9").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["servers"][0]["server"], "2406:da18:c4a:2281::1");
        assert_eq!(v["servers"][1]["server"], "203.0.113.9");

        // IPv6 aws -> only the IPv6 entry changes.
        let out6 = substitute_ss_server(conf, "2001:db8::2").unwrap();
        let v6: serde_json::Value = serde_json::from_str(&out6).unwrap();
        assert_eq!(v6["servers"][0]["server"], "2001:db8::2");
        assert_eq!(v6["servers"][1]["server"], "54.179.191.126");

        assert!(is_ipv4("54.179.191.126"));
        assert!(!is_ipv4("2001:db8::2"));
        assert!(!is_ipv4("256.1.1.1"));
        assert!(!is_ipv4("example.com"));
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/a b"), "'/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn ssh_dest_builds_user_at_host() {
        let host = CollectorHost {
            hostname: "1.2.3.4".to_string(),
            user: "root".to_string(),
            ..Default::default()
        };
        assert_eq!(SshConn::from_host(&host).dest(), "root@1.2.3.4");
        let no_user = CollectorHost {
            hostname: "h".to_string(),
            ..Default::default()
        };
        assert_eq!(SshConn::from_host(&no_user).dest(), "h");
    }

    #[test]
    fn legacy_split_parts_are_removed() {
        let dir = std::env::temp_dir().join(format!("collector_split_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let file = dir.join("blob.bin");
        fs::write(&file, b"data").unwrap();
        fs::write(dir.join("blob.bin.autosplit.000"), b"part0").unwrap();
        fs::write(dir.join("blob.bin.autosplit.001"), b"part1").unwrap();
        remove_existing_parts(&file).unwrap();
        assert!(!dir.join("blob.bin.autosplit.000").exists());
        assert!(!dir.join("blob.bin.autosplit.001").exists());
        assert!(file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitignore_editor_removes_anchored_path() {
        let dir = std::env::temp_dir().join(format!("collector_ignore_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join(".gitignore"),
            "/aws/usr/local/big.bin\n/other/path\n",
        )
        .unwrap();
        let mut editor = GitignoreEditor::load(&dir).unwrap();
        editor.remove(Path::new("aws/usr/local/big.bin"));
        editor.save().unwrap();
        let body = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(body.matches("/aws/usr/local/big.bin").count(), 0);
        assert!(body.contains("/other/path"));
        let _ = fs::remove_dir_all(&dir);
    }
}
