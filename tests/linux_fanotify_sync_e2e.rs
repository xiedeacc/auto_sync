#![cfg(target_os = "linux")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use auto_sync::core::config::{
    AppConfig, DestinationConfig, ScheduleConfig, ScheduleMode, SnapshotBackend, SnapshotConfig,
    SourceGroupConfig, SyncMode,
};
use auto_sync::core::state::State;
use auto_sync::core::sync::{SyncRequestMode, sync_all_pending, sync_destination_now_with_mode};
use auto_sync::core::watcher::{record_startup_mtime_events, spawn_source_watcher_thread};
use chrono::Utc;
use filetime::{FileTime, set_file_mtime};

const SOURCE_ID: &str = "linux_fanotify_src";
const DESTINATION_ID: &str = "linux_fanotify_dst";
static TRACE_INIT: Once = Once::new();

fn init_trace() {
    TRACE_INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .try_init();
    });
}

#[test]
fn fanotify_full_sync_then_realtime_incremental_syncs_event_paths() -> Result<()> {
    init_trace();
    let env = TestEnv::new("fanotify_incremental")?;
    write_bytes(env.src.join("changed.txt"), b"v1")?;
    write_bytes(env.src.join("delete_me.txt"), b"delete")?;
    write_bytes(env.src.join("move_me.txt"), b"move")?;
    write_bytes(env.src.join("untouched.txt"), b"untouched")?;
    fs::create_dir_all(env.src.join("existing_dir"))?;

    let cfg = env.config();
    let mut state = State::open(&env.db)?;
    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )?;
    assert_file(&env.effective_dst().join("changed.txt"), b"v1")?;

    write_bytes(env.effective_dst().join("destination-only.txt"), b"extra")?;
    let watcher = WatcherGuard::start(&cfg, &env.db);
    thread::sleep(Duration::from_millis(500));

    write_bytes(env.src.join("changed.txt"), b"v2 changed on zfs")?;
    write_bytes(env.src.join("existing_dir/new.txt"), b"new")?;
    fs::create_dir_all(env.src.join("empty_created_dir"))?;
    fs::remove_file(env.src.join("delete_me.txt"))?;
    fs::rename(env.src.join("move_me.txt"), env.src.join("moved.txt"))?;
    wait_for_event_paths(
        &state,
        &[
            "changed.txt",
            "existing_dir/new.txt",
            "empty_created_dir",
            "delete_me.txt",
            "move_me.txt",
            "moved.txt",
        ],
        Duration::from_secs(10),
    )?;

    assert_eq!(state.advance_due_destination_targets(&cfg)?.len(), 1);
    sync_all_pending(&cfg, &mut state)?;
    drop(watcher);

    assert_file(
        &env.effective_dst().join("changed.txt"),
        b"v2 changed on zfs",
    )?;
    assert_file(&env.effective_dst().join("existing_dir/new.txt"), b"new")?;
    assert!(env.effective_dst().join("empty_created_dir").is_dir());
    assert!(!env.effective_dst().join("delete_me.txt").exists());
    assert!(!env.effective_dst().join("move_me.txt").exists());
    assert_file(&env.effective_dst().join("moved.txt"), b"move")?;
    assert_file(&env.effective_dst().join("untouched.txt"), b"untouched")?;
    assert_file(&env.effective_dst().join("destination-only.txt"), b"extra")?;
    assert_green(&state, &cfg)?;
    Ok(())
}

#[test]
fn startup_mtime_scan_backfills_events_missed_while_fanotify_stopped() -> Result<()> {
    init_trace();
    let env = TestEnv::new("startup_mtime_backfill")?;
    write_bytes(env.src.join("already_synced.txt"), b"base")?;

    let cfg = env.config();
    let mut state = State::open(&env.db)?;
    sync_destination_now_with_mode(
        &cfg,
        &mut state,
        SOURCE_ID,
        DESTINATION_ID,
        SyncRequestMode::Full,
    )?;
    assert_file(&env.effective_dst().join("already_synced.txt"), b"base")?;
    write_bytes(env.effective_dst().join("destination-only.txt"), b"extra")?;

    state.record_event(SOURCE_ID, 0, "modify", Some("already_synced.txt"), false)?;
    let cutoff = Utc::now();
    rusqlite::Connection::open(&env.db)?.execute(
        "UPDATE event_log SET observed_at=?1, persisted_at=?1",
        [cutoff.to_rfc3339()],
    )?;

    let missed = env.src.join("missed_while_stopped.txt");
    write_bytes(missed.clone(), b"missed")?;
    let future = FileTime::from_unix_time(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64 + 60,
        0,
    );
    set_file_mtime(&missed, future)?;

    let recorded = record_startup_mtime_events(&cfg, &state)?;
    assert_eq!(recorded, 1);
    let cycle_id = state
        .current_open_cycle_id(SOURCE_ID)?
        .context("missing open cycle")?;
    let events = state.cycle_events(SOURCE_ID, cycle_id)?;
    assert!(events.iter().any(|event| {
        event.event_kind == "startup_mtime_scan"
            && event.rel_path.as_deref() == Some("missed_while_stopped.txt")
    }));

    assert_eq!(state.advance_due_destination_targets(&cfg)?.len(), 1);
    sync_all_pending(&cfg, &mut state)?;

    assert_file(
        &env.effective_dst().join("missed_while_stopped.txt"),
        b"missed",
    )?;
    assert_file(&env.effective_dst().join("destination-only.txt"), b"extra")?;
    assert_green(&state, &cfg)?;
    Ok(())
}

struct TestEnv {
    base: PathBuf,
    src: PathBuf,
    dst: PathBuf,
    db: PathBuf,
}

impl TestEnv {
    fn new(name: &str) -> Result<Self> {
        let base = unique_tmp_dir(name);
        fs::create_dir_all(&base)?;
        let src = base.join("src");
        let dst = base.join("dst");
        fs::create_dir_all(&src)?;
        fs::create_dir_all(&dst)?;
        Ok(Self {
            db: base.join("state.sqlite"),
            base,
            src,
            dst,
        })
    }

    fn config(&self) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.app.data_db = self.db.clone();
        cfg.source_groups = vec![SourceGroupConfig {
            id: SOURCE_ID.to_string(),
            machine_id: "local".to_string(),
            src: self.src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: DESTINATION_ID.to_string(),
                machine_id: "local".to_string(),
                path: self.dst.clone(),
                enabled: true,
                paused: false,
                schedule: ScheduleConfig {
                    mode: ScheduleMode::Realtime,
                    ..ScheduleConfig::default()
                },
                sync: None,
            }],
        }];
        cfg
    }

    fn effective_dst(&self) -> PathBuf {
        self.dst.join("src")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let root = test_root();
        if self.base.starts_with(&root)
            && self.base.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("auto_sync_linux_fanotify_")
            })
        {
            let _ = fs::remove_dir_all(&self.base);
        }
    }
}

struct WatcherGuard {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl WatcherGuard {
    fn start(cfg: &AppConfig, db_path: &Path) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle =
            spawn_source_watcher_thread(cfg.clone(), db_path.to_path_buf(), shutdown.clone());
        Self {
            shutdown,
            handle: Some(handle),
        }
    }
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn wait_for_event_paths(state: &State, paths: &[&str], timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(cycle_id) = state.current_open_cycle_id(SOURCE_ID)? {
            let events = state.cycle_events(SOURCE_ID, cycle_id)?;
            if paths.iter().all(|path| {
                events
                    .iter()
                    .any(|event| event.rel_path.as_deref() == Some(*path))
            }) {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "fanotify did not record expected paths within {:?}",
        timeout
    )
}

fn assert_green(state: &State, cfg: &AppConfig) -> Result<()> {
    let view = state
        .destination_views(cfg)?
        .into_iter()
        .find(|view| view.destination_id == DESTINATION_ID)
        .context("missing destination view")?;
    if view.status != "green" {
        bail!(
            "destination status is {}: {}",
            view.status,
            view.status_reason
        );
    }
    Ok(())
}

fn unique_tmp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    test_root().join(format!(
        "auto_sync_linux_fanotify_{name}_{}_{nanos}",
        std::process::id()
    ))
}

fn test_root() -> PathBuf {
    std::env::var_os("AUTO_SYNC_LINUX_FANOTIFY_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/zfs/tmp"))
}

fn write_bytes(path: PathBuf, value: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, value)?;
    Ok(())
}

fn assert_file(path: &Path, expected: &[u8]) -> Result<()> {
    let actual = fs::read(path).with_context(|| format!("missing {}", path.display()))?;
    if actual != expected {
        bail!(
            "content mismatch at {} (len {} vs {})",
            path.display(),
            actual.len(),
            expected.len()
        );
    }
    Ok(())
}
