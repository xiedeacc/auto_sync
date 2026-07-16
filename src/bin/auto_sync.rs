#![cfg_attr(windows, windows_subsystem = "windows")]

//! Unified `auto_sync` daemon: it runs the scheduler + file watcher and the web
//! server in one process. The desktop window is provided by the Flutter
//! `auto_sync_gui` app, which attaches to this process over the existing HTTP
//! API.

#[cfg(all(unix, not(target_env = "musl")))]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use auto_sync::core::backend::Backend;
use auto_sync::core::config::{AppConfig, load_config, load_or_create_config};
use auto_sync::core::logging::init_logging;
use auto_sync::core::state::State;
use auto_sync::core::sync::sync_all_pending;
use auto_sync::core::watcher::{
    source_is_watched_here, spawn_source_watcher_thread, watcher_covers_downtime,
};
use auto_sync::core::web_api;
use clap::Parser;
use tracing::{error, info, warn};

#[cfg(windows)]
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

#[derive(Debug, Parser)]
#[command(name = "auto_sync")]
#[command(about = "auto_sync — directory sync daemon and web UI backend")]
struct Args {
    /// Path to the config file. If omitted, auto_sync looks for
    /// conf/auto_sync.toml relative to the current directory, then relative to
    /// the executable (so launching from bin\ finds the repo config one level up).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Deprecated compatibility flag; the Flutter desktop UI is a separate process.
    #[arg(long)]
    no_gui: bool,
    /// Deprecated compatibility flag; the Flutter desktop UI handles its own visibility.
    #[arg(long)]
    hidden: bool,
    /// Internal guard to avoid repeated UAC relaunch attempts.
    #[cfg(windows)]
    #[arg(long, hide = true)]
    elevation_attempted: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_arg = auto_sync::core::config::resolve_config_path(args.config.as_deref());
    let cfg = load_or_create_config(&config_arg)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync")?;
    info!(config = %config_arg.display(), "auto_sync starting");
    #[cfg(windows)]
    if maybe_relaunch_elevated_on_windows(&args, &config_arg)? {
        return Ok(());
    }

    let config_path = config_arg
        .canonicalize()
        .unwrap_or_else(|_| config_arg.clone());

    // Only one auto_sync process may run against a given install at a time --
    // two live instances would each run their own scheduler/watcher and race
    // on the shared destinations. If another instance already holds the lock,
    // stop it and take over rather than refusing to start. Held for the rest
    // of the process lifetime; the OS releases it automatically on exit, even
    // a forced kill.
    let lock_path = config_path
        .parent()
        .map(|dir| dir.join(".auto_sync.lock"))
        .unwrap_or_else(|| PathBuf::from(".auto_sync.lock"));
    let _instance_lock = match acquire_single_instance_lock(&lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            // A GUI-subsystem build has no console, so a plain returned Err
            // would otherwise vanish silently instead of reaching the user.
            error!(error = %err, "refusing to start: could not stop the previous auto_sync instance");
            // Still propagate the failure so launchers (schtasks, scripts)
            // see a nonzero exit instead of a phantom success.
            return Err(err);
        }
    };

    let process_started_at = chrono::Utc::now();
    auto_sync::core::backend::set_process_started_at(process_started_at);
    State::open(&cfg.app.data_db)
        .and_then(|state| state.record_process_started_at(process_started_at))
        .context("failed to record process start time")?;

    // Apply receiver-side policy up front so the web server (the destination of
    // pushes) honours it even though it never runs the scheduler loop.
    auto_sync::core::sync::configure_fsync(cfg.app.sync.fsync);

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .context("failed to install Ctrl-C handler")?;
    }

    // Push local status changes (cycle advances, verify results) to peer
    // machines so their UIs update within ~2s instead of a full poll cycle.
    auto_sync::core::peer_notify::spawn_notifier(config_path.clone(), shutdown.clone());

    // Scheduler + watcher always run, on a background thread.
    let scheduler = {
        let cfg = cfg.clone();
        let config_path = config_path.clone();
        let shutdown = shutdown.clone();
        thread::Builder::new()
            .name("auto_sync_scheduler".to_string())
            .spawn(move || {
                if let Err(err) = run_scheduler(config_path, cfg, shutdown) {
                    error!(error = %err, "scheduler loop exited with error");
                }
            })
            .context("failed to start scheduler thread")?
    };

    let backend = Backend::new(config_path, cfg.app.port);
    run_foreground(backend, cfg.app.port, &args);

    shutdown.store(true, Ordering::SeqCst);
    let _ = scheduler.join();
    info!("auto_sync stopped");
    Ok(())
}

/// Holds the OS-level exclusive lock on the single-instance lock file for the
/// life of the process. Dropping (or the process exiting, even via a kill)
/// releases it automatically.
struct InstanceLock {
    _file: std::fs::File,
}

/// Ensure only one auto_sync process runs against this install at a time. If
/// another instance already holds the lock, verify it's really an auto_sync
/// process (by PID + image name, to avoid killing an unrelated recycled PID),
/// stop it, and take the lock over rather than refusing to start.
fn acquire_single_instance_lock(path: &std::path::Path) -> Result<InstanceLock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open lock file {}", path.display()))?;

    if try_lock_file(&file)? {
        write_lock_pid(&mut file, path)?;
        return Ok(InstanceLock { _file: file });
    }

    match read_lock_pid(path) {
        Some(pid) if pid == std::process::id() => {}
        Some(pid) if process_is_auto_sync(pid) => {
            warn!(pid, "another auto_sync instance is running; stopping it");
            kill_process(pid);
        }
        // Unknown or stale holder PID: the lock is definitely held by some
        // auto_sync process, so fall back to stopping every other auto_sync
        // process by image name (otherwise the takeover promised above can
        // never happen).
        other => {
            warn!(
                lock_pid = ?other,
                "lock holder PID unknown or stale; stopping other auto_sync processes by name"
            );
            kill_other_auto_sync_processes();
        }
    }

    for _ in 0..25 {
        thread::sleep(Duration::from_millis(200));
        if try_lock_file(&file)? {
            write_lock_pid(&mut file, path)?;
            return Ok(InstanceLock { _file: file });
        }
    }

    anyhow::bail!(
        "another auto_sync instance still holds {} after attempting to stop it",
        path.display()
    );
}

/// Sidecar holding the lock holder's PID in a file nobody range-locks. On
/// Windows the holder's LockFileEx exclusive lock blocks other processes from
/// reading the lock file itself, so a takeover could never learn which PID to
/// stop (it always hit the "PID could not be read" path and gave up).
fn lock_pid_sidecar(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".pid");
    PathBuf::from(name)
}

fn read_lock_pid(path: &std::path::Path) -> Option<u32> {
    let parse = |text: String| text.trim().parse().ok();
    std::fs::read_to_string(path)
        .ok()
        .and_then(parse)
        .or_else(|| {
            std::fs::read_to_string(lock_pid_sidecar(path))
                .ok()
                .and_then(parse)
        })
}

fn write_lock_pid(file: &mut std::fs::File, path: &std::path::Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    // Best-effort: the sidecar is only a hint for a future takeover; failing
    // to write it must not block startup.
    let _ = std::fs::write(lock_pid_sidecar(path), std::process::id().to_string());
    Ok(())
}

#[cfg(windows)]
fn try_lock_file(file: &std::fs::File) -> Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    Ok(ok != 0)
}

#[cfg(unix)]
fn try_lock_file(file: &std::fs::File) -> Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok(rc == 0)
}

#[cfg(windows)]
fn process_is_auto_sync(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .to_lowercase()
            .contains("auto_sync.exe"),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn process_is_auto_sync(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim() == "auto_sync")
        .unwrap_or(false)
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

/// Last-resort takeover path when the lock holder's PID can't be determined:
/// stop every other auto_sync process by image name (same semantics as the
/// deploy scripts' stop step).
#[cfg(windows)]
fn kill_other_auto_sync_processes() {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq auto_sync.exe", "/FO", "CSV", "/NH"])
        .output();
    let Ok(out) = output else { return };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        // CSV row: "auto_sync.exe","11184","Console","1","123,456 K"
        let mut fields = line
            .split('"')
            .filter(|part| !part.is_empty() && *part != ",");
        let Some(image) = fields.next() else { continue };
        if !image.eq_ignore_ascii_case("auto_sync.exe") {
            continue;
        }
        let Some(pid) = fields.next().and_then(|raw| raw.parse::<u32>().ok()) else {
            continue;
        };
        if pid != std::process::id() {
            kill_process(pid);
        }
    }
}

#[cfg(unix)]
fn kill_other_auto_sync_processes() {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && process_is_auto_sync(pid) {
            kill_process(pid);
        }
    }
}

#[cfg(windows)]
fn maybe_relaunch_elevated_on_windows(args: &Args, config_arg: &std::path::Path) -> Result<bool> {
    if args.elevation_attempted || unsafe { IsUserAnAdmin() } != 0 {
        return Ok(false);
    }
    warn!("auto_sync is not elevated on Windows; relaunching with UAC");

    let exe = std::env::current_exe().context("failed to locate current executable")?;
    let working_dir = std::env::current_dir().context("failed to locate current directory")?;
    let config = config_arg
        .canonicalize()
        .unwrap_or_else(|_| config_arg.to_path_buf());
    let mut relaunch_args = vec![
        OsString::from("--config"),
        config.into_os_string(),
        OsString::from("--elevation-attempted"),
    ];
    if args.no_gui {
        relaunch_args.push(OsString::from("--no-gui"));
    }
    if args.hidden {
        relaunch_args.push(OsString::from("--hidden"));
    }
    let parameters = join_windows_command_line(&relaunch_args);
    let operation = wide_null(OsStr::new("runas"));
    let file = wide_null(exe.as_os_str());
    let params = wide_null(OsStr::new(&parameters));
    let dir = wide_null(working_dir.as_os_str());
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            params.as_ptr(),
            dir.as_ptr(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        warn!(
            shell_execute_result = result,
            "failed to relaunch auto_sync elevated; continuing without elevation"
        );
        return Ok(false);
    }
    info!("elevated auto_sync relaunch requested; exiting non-elevated process");
    Ok(true)
}

#[cfg(windows)]
fn join_windows_command_line(args: &[OsString]) -> String {
    args.iter()
        .map(|arg| quote_windows_arg(&arg.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(windows)]
fn quote_windows_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0_usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

#[cfg(windows)]
fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn quotes_windows_relaunch_arguments() {
        assert_eq!(
            quote_windows_arg(r#"C:\sync root\conf.toml"#),
            r#""C:\sync root\conf.toml""#
        );
        assert_eq!(
            quote_windows_arg(r#"C:\path\"quote".toml"#),
            r#""C:\path\\\"quote\".toml""#
        );
    }
}

/// Run the web server. The Flutter desktop shell connects to this HTTP API.
fn run_foreground(backend: Backend, port: u16, args: &Args) {
    let _ = args;
    run_web_only(backend, port);
}

fn run_web_only(backend: Backend, port: u16) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(error = %err, "failed to create web runtime");
            return;
        }
    };
    if let Err(err) = runtime.block_on(web_api::serve(backend, port)) {
        error!(error = %err, "web server stopped");
    }
}

// ---------------------------------------------------------------------------
// Scheduler + watcher loop (always runs)
// ---------------------------------------------------------------------------

fn run_scheduler(
    config_path: PathBuf,
    initial_cfg: AppConfig,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut cfg = initial_cfg;
    let mut state = State::open(&cfg.app.data_db)?;
    state.ensure_config(&cfg)?;
    state.ensure_open_cycles(&cfg)?;
    match state.abort_stale_running_tasks() {
        Ok(aborted) if aborted > 0 => {
            info!(
                aborted,
                "marked task-log rows orphaned by the previous run as aborted"
            );
        }
        Ok(_) => {}
        Err(err) => warn!(error = %err, "failed to sweep stale running tasks"),
    }

    // Start the watcher and wait for its marks to be live, then raise the
    // restart notice: on platforms without a persistent journal (fanotify)
    // whatever changed while the process was down is unobservable, and the
    // user decides how to reconcile (Compare / Full) instead
    // of paying an automatic full-tree scan on every restart.
    let mut watcher_state = start_watcher(&cfg);
    wait_for_watcher_armed(&shutdown);
    raise_restart_notices(&cfg, &state);

    let mut watcher_signature = config_signature(&cfg);
    let mut last_status_log =
        Instant::now() - Duration::from_secs(cfg.app.status_log_interval_secs);

    while !shutdown.load(Ordering::SeqCst) {
        match load_config(&config_path) {
            Ok(new_cfg) => {
                let signature = config_signature(&new_cfg);
                if signature != watcher_signature {
                    info!("source config changed; restarting source watcher");
                    stop_watcher(&mut watcher_state);
                    watcher_state = start_watcher(&new_cfg);
                    wait_for_watcher_armed(&shutdown);
                    watcher_signature = signature;
                    // fanotify has no persistent journal: events during the
                    // stop→re-arm window are unobservable. Mark the gap so the
                    // next pass reconciles instead of trusting the stream.
                    if !watcher_covers_downtime() {
                        for source in new_cfg
                            .source_groups
                            .iter()
                            .filter(|s| s.enabled && source_is_watched_here(s))
                        {
                            if let Err(err) =
                                state.record_event(&source.id, 0, "watcher_restart_gap", None, true)
                            {
                                warn!(source = source.id, error = %err,
                                    "failed to record watcher restart gap");
                            }
                        }
                    }
                }
                cfg = new_cfg;
            }
            Err(err) => warn!(error = %err, "failed to reload config; keeping previous config"),
        }

        if let Err(err) = state.ensure_config(&cfg) {
            error!(error = %err, "failed to persist config into state db");
        }
        match state.advance_due_destination_targets(&cfg) {
            Ok(closed) => {
                for cycle in closed {
                    info!(
                        source = cycle.source_id,
                        cycle_id = cycle.id,
                        "cycle target advanced"
                    );
                }
            }
            Err(err) => error!(error = %err, "failed to advance due destination targets"),
        }

        if let Err(err) = sync_all_pending(&cfg, &mut state) {
            error!(error = %err, "sync pending cycles failed");
        }

        // Standby lifecycle: open/close each cold-backup pool's wake window and
        // spin its disks down. A pool is kept awake while it is "busy": a sync
        // is running (never park mid-write), OR pending work whose source+dest
        // pools are ALL currently in their windows still needs it. Once that
        // drains, the pool parks early rather than idling out the whole window.
        if !cfg.standby_pools.is_empty() {
            use auto_sync::core::standby::{gate_for_sync, pool_covers_path};
            let now = chrono::Local::now();
            let syncing = auto_sync::core::sync::sync_is_running();
            let busy = |pool_name: &str| -> bool {
                if syncing {
                    return true; // a sync is in flight — don't stop any disk mid-write
                }
                let Some(pool) = cfg.standby_pools.iter().find(|p| p.name == pool_name) else {
                    return false;
                };
                for src in cfg.source_groups.iter().filter(|s| s.enabled) {
                    for dst in src.destinations.iter().filter(|d| d.enabled && !d.paused) {
                        let (src_root, dst_root) = (src.src.as_path(), dst.path.as_path());
                        if !pool_covers_path(pool, src_root) && !pool_covers_path(pool, dst_root) {
                            continue;
                        }
                        // Only work that can actually run now (its gating pool is
                        // in-window) keeps this pool up — including a source pool
                        // pulled awake on demand for a chained backup. Work whose
                        // gating destination is still parked does not.
                        if !matches!(
                            gate_for_sync(&cfg.standby_pools, src_root, dst_root, now),
                            Ok(None)
                        ) {
                            continue;
                        }
                        // Pending due work = the destination's target cycle is
                        // ahead of its last verified cycle. Unreadable → assume
                        // busy (keep the disk awake rather than risk an early park).
                        match state.destination_offset(&src.id, &dst.id) {
                            Ok(off) => {
                                if let Some(target) = off.target_cycle_id {
                                    if off.last_verified_cycle_id < Some(target) {
                                        return true;
                                    }
                                }
                            }
                            Err(_) => return true,
                        }
                    }
                }
                false
            };
            auto_sync::core::standby::tick(&cfg.standby_pools, busy);
        }

        if last_status_log.elapsed() >= Duration::from_secs(cfg.app.status_log_interval_secs) {
            log_destination_status(&state, &cfg);
            last_status_log = Instant::now();
        }

        // Wake immediately when a watcher records an event (realtime latency
        // in milliseconds); the 5s timeout remains the schedule heartbeat.
        auto_sync::core::signal::wait_for_activity(Duration::from_secs(5));
    }

    stop_watcher(&mut watcher_state);
    Ok(())
}

/// On platforms whose watcher cannot see downtime changes (fanotify), raise
/// a persistent per-source notice: the user reconciles manually (Compare /
/// Full) or dismisses it. Windows USN replays its persistent
/// journal across restarts, so nothing was missed and no notice is raised.
fn raise_restart_notices(cfg: &AppConfig, state: &State) {
    if watcher_covers_downtime() {
        return;
    }
    for source in cfg
        .source_groups
        .iter()
        .filter(|source| source_is_watched_here(source))
    {
        match state.raise_restart_notice(&source.id) {
            Ok(true) => info!(
                source = source.id,
                "daemon restarted: changes made while it was down may be unrecorded; \
                 run Compare/Full or dismiss the notice"
            ),
            Ok(false) => {}
            Err(err) => warn!(source = source.id, error = %err, "failed to raise restart notice"),
        }
    }
}

fn log_destination_status(state: &State, cfg: &AppConfig) {
    match state.destination_views(cfg) {
        Ok(views) => {
            for view in views {
                info!(
                    source = view.source_id,
                    destination = view.destination_id,
                    path = view.path,
                    latest_cycle = view.latest_closed_cycle_id.unwrap_or_default(),
                    verified_cycle = view.last_verified_cycle_id.unwrap_or_default(),
                    status = view.status,
                    reason = view.status_reason,
                    "destination status"
                );
            }
        }
        Err(err) => error!(error = %err, "failed to load destination status"),
    }
}

struct WatcherState {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

fn start_watcher(cfg: &AppConfig) -> WatcherState {
    auto_sync::core::signal::reset_watcher_armed();
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_source_watcher_thread(cfg.clone(), cfg.app.data_db.clone(), stop.clone());
    WatcherState {
        stop,
        handle: Some(handle),
    }
}

/// Block until the watcher backend reports its marks/journals live (bounded;
/// the fanotify prebuild fallback can take minutes on a huge tree). On
/// timeout the startup scan proceeds anyway — a bounded risk beats never
/// scanning.
fn wait_for_watcher_armed(shutdown: &Arc<AtomicBool>) {
    let deadline = Instant::now() + Duration::from_secs(900);
    while !shutdown.load(Ordering::SeqCst) {
        if auto_sync::core::signal::wait_watcher_armed(Duration::from_millis(500)) {
            return;
        }
        if Instant::now() >= deadline {
            warn!("watcher did not report armed in time; running the startup scan anyway");
            return;
        }
    }
}

fn stop_watcher(state: &mut WatcherState) {
    state.stop.store(true, Ordering::SeqCst);
    if let Some(handle) = state.handle.take() {
        if let Err(err) = handle.join() {
            warn!(?err, "source watcher thread join failed");
        }
    }
}

/// Only the fields the watcher actually depends on. Hashing the whole config
/// restarted the watcher (losing the fanotify queue) on every unrelated save:
/// discovery-thread machine metadata refreshes, UI preferences, aliases.
fn config_signature(cfg: &AppConfig) -> String {
    let mut parts = vec![cfg.app.data_db.display().to_string()];
    for source in &cfg.source_groups {
        parts.push(format!(
            "{}|{}|{}|{}|{}|{}",
            source.id,
            source.machine_id,
            source.src.display(),
            source.enabled,
            source.add_directory,
            source
                .destinations
                .iter()
                .map(|dst| format!("{}:{}", dst.id, dst.enabled))
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    parts.join("\n")
}
