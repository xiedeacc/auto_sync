use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::core::config::{AppConfig, ScheduleMode, SourceGroupConfig, machine_id_or_local};
use crate::core::state::State;

#[cfg(target_os = "linux")]
pub mod fanotify;

#[cfg(target_os = "windows")]
pub mod windows_usn;

#[cfg(target_os = "linux")]
pub use fanotify::spawn_source_watcher_thread;

#[cfg(target_os = "windows")]
pub use windows_usn::spawn_source_watcher_thread;

pub fn record_startup_mtime_events(cfg: &AppConfig, state: &State) -> Result<usize> {
    let mut total = 0_usize;
    for source in cfg
        .source_groups
        .iter()
        .filter(|source| source_needs_startup_scan(source))
    {
        total += record_source_startup_mtime_events(source, state)
            .with_context(|| format!("failed to scan startup changes for source {}", source.id))?;
    }
    Ok(total)
}

fn source_needs_startup_scan(source: &SourceGroupConfig) -> bool {
    source.enabled
        && machine_id_or_local(&source.machine_id) == "local"
        && source
            .destinations
            .iter()
            .any(|dst| dst.enabled && dst.schedule.mode == ScheduleMode::Realtime)
}

fn record_source_startup_mtime_events(source: &SourceGroupConfig, state: &State) -> Result<usize> {
    let Some(cutoff) = state.latest_event_observed_at(&source.id)? else {
        return Ok(0);
    };
    let seen_paths = state.event_paths_observed_since(&source.id, cutoff)?;
    let root = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    if root.is_file() {
        return record_file_source_startup_mtime_event(source, state, &root, cutoff, &seen_paths);
    }
    let mut recorded = 0_usize;

    for entry in WalkDir::new(&root).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if path == root {
            continue;
        }
        if entry_is_excluded(&root, path, &source.excludes) {
            continue;
        }
        let rel_path = path
            .strip_prefix(&root)
            .with_context(|| format!("failed to strip root from {}", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        if rel_path.is_empty() || seen_paths.contains(&rel_path) {
            continue;
        }
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("failed to read metadata {}", path.display()))?;
        if metadata_mtime_after(metadata.modified()?, cutoff) {
            state.record_event(&source.id, 0, "startup_mtime_scan", Some(&rel_path), false)?;
            recorded += 1;
        }
    }

    if recorded > 0 {
        tracing::info!(
            source = source.id,
            recorded,
            cutoff = %cutoff,
            "startup mtime scan recorded missed realtime events"
        );
    }
    Ok(recorded)
}

fn record_file_source_startup_mtime_event(
    source: &SourceGroupConfig,
    state: &State,
    root: &Path,
    cutoff: DateTime<Utc>,
    seen_paths: &std::collections::HashSet<String>,
) -> Result<usize> {
    let Some(file_name) = root.file_name() else {
        return Ok(0);
    };
    let rel_path = file_name.to_string_lossy().replace('\\', "/");
    if seen_paths.contains(&rel_path) {
        return Ok(0);
    }
    let metadata = fs::symlink_metadata(root)
        .with_context(|| format!("failed to read metadata {}", root.display()))?;
    if metadata_mtime_after(metadata.modified()?, cutoff) {
        state.record_event(&source.id, 0, "startup_mtime_scan", Some(&rel_path), false)?;
        return Ok(1);
    }
    Ok(0)
}

fn metadata_mtime_after(modified: std::time::SystemTime, cutoff: DateTime<Utc>) -> bool {
    DateTime::<Utc>::from(modified) > cutoff
}

fn entry_is_excluded(root: &Path, path: &Path, excludes: &[PathBuf]) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    excludes
        .iter()
        .any(|exclude| rel == exclude || rel.starts_with(exclude))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn spawn_source_watcher_thread(
    _cfg: crate::core::config::AppConfig,
    _db_path: std::path::PathBuf,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    let _ = (PathBuf::new(), Arc::<AtomicBool>::clone(&shutdown));
    thread::spawn(move || {
        tracing::info!(
            "realtime source watcher is not available on this platform; using periodic reconciliation"
        );
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{
        AppConfig, DestinationConfig, ScheduleConfig, ScheduleMode, SourceGroupConfig, SyncMode,
    };

    #[test]
    fn startup_mtime_scan_records_paths_changed_after_last_event() {
        let temp = temp_dir("startup_mtime_scan");
        let src = temp.join("src");
        let dst = temp.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = temp.join("state.sqlite");
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: Default::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig {
                    mode: ScheduleMode::Realtime,
                    ..ScheduleConfig::default()
                },
                sync: None,
            }],
        });

        let state = State::open(&cfg.app.data_db).unwrap();
        state.ensure_config(&cfg).unwrap();
        state
            .record_event("src_1", 0, "modify", Some("old.txt"), false)
            .unwrap();
        let old_time = Utc::now() - chrono::Duration::try_seconds(60).unwrap();
        rusqlite::Connection::open(&cfg.app.data_db)
            .unwrap()
            .execute(
                "UPDATE event_log SET observed_at=?1, persisted_at=?1",
                [old_time.to_rfc3339()],
            )
            .unwrap();

        fs::write(src.join("changed.txt"), b"changed").unwrap();

        let recorded = record_startup_mtime_events(&cfg, &state).unwrap();

        assert_eq!(recorded, 1);
        let cycle_id = state.current_open_cycle_id("src_1").unwrap().unwrap();
        let events = state.cycle_events("src_1", cycle_id).unwrap();
        assert!(events.iter().any(|event| {
            event.event_kind == "startup_mtime_scan"
                && event.rel_path.as_deref() == Some("changed.txt")
        }));
        fs::remove_dir_all(temp).ok();
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("auto_sync_{name}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
