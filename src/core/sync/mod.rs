use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use filetime::{FileTime, set_file_mtime};
use tracing::{error, info, warn};
use walkdir::{DirEntry, WalkDir};

use crate::core::config::{AppConfig, SourceGroupConfig};
use crate::core::state::{Cycle, SnapshotEntry, State};
use crate::core::status::check_destination_online;

const INTERNAL_TMP: &str = ".auto_sync_tmp";
const INTERNAL_TRASH: &str = ".auto_sync_trash";
const INTERNAL_PROBE: &str = ".auto_sync_probe";

pub fn sync_all_pending(cfg: &AppConfig, state: &mut State) -> Result<()> {
    state.ensure_config(cfg)?;
    for source in cfg.source_groups.iter().filter(|s| s.enabled) {
        let cycles = state.closed_cycles_for_source(&source.id)?;
        for cycle in cycles {
            sync_cycle_for_source(cfg, state, source, &cycle)?;
        }
    }
    Ok(())
}

pub fn sync_cycle_for_source(
    _cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<()> {
    info!(
        source = source.id,
        cycle_id = cycle.id,
        needs_full_rescan = cycle.needs_full_rescan,
        "sync cycle started"
    );

    if !source.src.exists() || !source.src.is_dir() {
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
        bail!("source path is unavailable: {}", source.src.display());
    }

    state.mark_cycle_status(cycle.id, "planning")?;
    let source_snapshot = take_snapshot(&source.src, SnapshotMode::Source)
        .with_context(|| format!("failed to snapshot source {}", source.src.display()))?;
    state.replace_snapshot(cycle.id, &source.id, &source_snapshot)?;

    state.mark_cycle_status(cycle.id, "syncing")?;
    let mut all_verified = true;
    for dst in source.destinations.iter().filter(|d| d.enabled) {
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

        match check_destination_online(&dst.path) {
            Ok(()) => {}
            Err(err) => {
                all_verified = false;
                warn!(
                    source = source.id,
                    destination = dst.id,
                    path = %dst.path.display(),
                    error = %err,
                    "destination offline"
                );
                state.upsert_destination_status(&source.id, &dst.id, None, "red", "dst_offline")?;
                continue;
            }
        }

        match sync_destination(&source.src, &dst.path, cycle.id, &source_snapshot) {
            Ok(()) => {
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
                error!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    error = %err,
                    "destination sync failed"
                );
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

    if all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(())
}

fn sync_destination(
    src_root: &Path,
    dst_root: &Path,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
) -> Result<()> {
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
        copy_entry(src_root, dst_root, cycle_id, entry)
            .with_context(|| format!("failed to copy {}", entry.rel_path))?;
    }

    let mut extra_paths: Vec<String> = dst_map
        .keys()
        .filter(|rel| !source_map.contains_key(*rel))
        .cloned()
        .collect();
    extra_paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
    for rel in extra_paths {
        move_to_trash(dst_root, &rel, cycle_id)
            .with_context(|| format!("failed to remove extra destination path {rel}"))?;
    }

    verify_destination(dst_root, source_snapshot)?;
    Ok(())
}

pub fn take_snapshot(root: &Path, mode: SnapshotMode) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    for item in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| should_visit(entry, mode))
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
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to read metadata {}", path.display()))?;
        let file_type = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else {
            continue;
        };
        let hash = match file_type {
            "file" => Some(hash_file(path)?),
            "symlink" => Some(hash_symlink(path)?),
            _ => None,
        };
        entries.push(SnapshotEntry {
            rel_path,
            file_type: file_type.to_string(),
            size: metadata.size() as i64,
            mtime_ns: metadata.mtime() * 1_000_000_000 + metadata.mtime_nsec(),
            mode: metadata.mode(),
            hash,
        });
    }
    Ok(entries)
}

#[derive(Debug, Clone, Copy)]
pub enum SnapshotMode {
    Source,
    Destination,
}

fn should_visit(entry: &DirEntry, mode: SnapshotMode) -> bool {
    if matches!(mode, SnapshotMode::Source) {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    name != INTERNAL_TMP && name != INTERNAL_TRASH && name != INTERNAL_PROBE
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
    symlink(&target, &tmp)
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

fn verify_destination(dst_root: &Path, source_snapshot: &[SnapshotEntry]) -> Result<()> {
    let expected = map_entries(source_snapshot);
    let actual_snapshot = take_snapshot(dst_root, SnapshotMode::Destination)?;
    let actual = map_entries(&actual_snapshot);
    for (rel, want) in &expected {
        match actual.get(rel) {
            Some(got) if entries_match(want, got) => {}
            Some(_) => bail!("destination mismatch at {rel}"),
            None => bail!("destination missing {rel}"),
        }
    }
    for rel in actual.keys() {
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
    perms.set_mode(mode);
    fs::set_permissions(path, perms)?;
    Ok(())
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

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn short_reason(err: &anyhow::Error) -> String {
    let text = err.to_string();
    text.chars().take(120).collect()
}
