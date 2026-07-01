#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

//! Unified `auto_sync` process: it always runs the scheduler + file watcher and
//! the web server in one process, and — on a desktop-capable build with a
//! display available — also opens the Tauri desktop window. Running everything
//! in a single process removes the old daemon/GUI contention over the shared
//! SQLite database and the destination machines.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use auto_sync::core::backend::Backend;
use auto_sync::core::config::{
    AppConfig, load_config, load_or_create_config, preferred_local_host,
};
use auto_sync::core::logging::init_logging;
use auto_sync::core::state::State;
use auto_sync::core::sync::sync_all_pending;
use auto_sync::core::watcher::{record_startup_mtime_events, spawn_source_watcher_thread};
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

#[cfg(feature = "gui")]
use auto_sync::core::backend::{BrowseResponse, RuntimeStatus, SyncActivityStatus};
#[cfg(feature = "gui")]
use auto_sync::core::config::MachineConfig;
#[cfg(feature = "gui")]
use auto_sync::core::machines::MachineStatus;
#[cfg(feature = "gui")]
use auto_sync::core::state::{DestinationView, ScanReport};
#[cfg(feature = "gui")]
use auto_sync::core::sync::SyncRequestMode;

#[derive(Debug, Parser)]
#[command(name = "auto_sync")]
#[command(about = "auto_sync — directory sync daemon, web UI, and optional desktop app")]
struct Args {
    /// Path to the config file. If omitted, auto_sync looks for
    /// conf/auto_sync.toml relative to the current directory, then relative to
    /// the executable (so launching from bin\ finds the repo config one level up).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Run web-only, never opening the desktop window even on a GUI build.
    #[arg(long)]
    no_gui: bool,
    /// Start with the main window hidden (tray only). Used by the autostart launcher.
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
            return Ok(());
        }
    };

    // Apply receiver-side policy up front so the web server (the destination of
    // pushes) honours it even though it never runs the scheduler loop.
    auto_sync::core::sync::configure_fsync(cfg.app.sync.fsync);

    let addr = bind_addr_for_port(cfg.app.port);

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .context("failed to install Ctrl-C handler")?;
    }

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

    let backend = Backend::new(config_path, addr.port());
    run_foreground(backend, addr, &args);

    shutdown.store(true, Ordering::SeqCst);
    let _ = scheduler.join();
    info!("auto_sync stopped");
    Ok(())
}

fn bind_addr_for_port(port: u16) -> SocketAddr {
    match preferred_local_host().parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port),
        Err(_) => SocketAddr::from(([0, 0, 0, 0], port)),
    }
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
        write_lock_pid(&mut file)?;
        return Ok(InstanceLock { _file: file });
    }

    match read_lock_pid(path) {
        Some(pid) if pid == std::process::id() => {}
        Some(pid) if process_is_auto_sync(pid) => {
            warn!(pid, "another auto_sync instance is running; stopping it");
            kill_process(pid);
        }
        Some(pid) => {
            warn!(
                pid,
                "lock file's PID is not an auto_sync process (stale/recycled); not killing it"
            );
        }
        None => {
            warn!("lock is held but its PID could not be read from the lock file");
        }
    }

    for _ in 0..25 {
        thread::sleep(Duration::from_millis(200));
        if try_lock_file(&file)? {
            write_lock_pid(&mut file)?;
            return Ok(InstanceLock { _file: file });
        }
    }

    anyhow::bail!(
        "another auto_sync instance still holds {} after attempting to stop it",
        path.display()
    );
}

fn read_lock_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_lock_pid(file: &mut std::fs::File) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
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

/// Run the web server, and the desktop window when one is available.
fn run_foreground(backend: Backend, addr: SocketAddr, args: &Args) {
    #[cfg(feature = "gui")]
    {
        if !args.no_gui && desktop_available() {
            run_with_desktop(backend, addr, args.hidden);
            return;
        }
        info!("no desktop session detected; running web-only");
    }
    #[cfg(not(feature = "gui"))]
    let _ = args;
    run_web_only(backend, addr);
}

fn run_web_only(backend: Backend, addr: SocketAddr) {
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
    if let Err(err) = runtime.block_on(web_api::serve(backend, addr)) {
        error!(error = %err, %addr, "web server stopped");
    }
}

#[cfg(feature = "gui")]
fn desktop_available() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
}

#[cfg(feature = "gui")]
fn spawn_web(backend: Backend, addr: SocketAddr) {
    let result = thread::Builder::new()
        .name("auto_sync_web".to_string())
        .spawn(move || run_web_only(backend, addr));
    if let Err(err) = result {
        warn!(error = %err, "failed to spawn web server thread");
    }
}

#[cfg(feature = "gui")]
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(true);

/// Create or remove the login autostart entry to match the setting. Best-effort:
/// failures are logged, not fatal. On Windows this manages the same Startup
/// launcher the deploy script writes; on Linux a ~/.config/autostart entry.
#[cfg(feature = "gui")]
fn apply_autostart(enabled: bool, config_path: &std::path::Path) {
    if let Err(err) = apply_autostart_inner(enabled, config_path) {
        warn!(error = %err, enabled, "failed to update autostart entry");
    }
}

/// Name of the Highest-privilege scheduled task used to start auto_sync at
/// logon without an interactive UAC prompt. Task Scheduler pre-authorizes the
/// elevation when the task is created (which itself requires the creating
/// process to already be elevated -- true here, since apply_autostart only
/// runs after the startup elevation check in main() has already completed).
#[cfg(all(feature = "gui", windows))]
const AUTOSTART_TASK_NAME: &str = "auto_sync";

#[cfg(all(feature = "gui", windows))]
fn apply_autostart_inner(enabled: bool, config_path: &std::path::Path) -> std::io::Result<()> {
    // The scheduled task's own "at logon" trigger is the entire autostart
    // mechanism: Task Scheduler launches a Highest-privilege task via its
    // trigger with no interactive UAC prompt. We used to also drop a
    // Startup-folder .vbs, but that only ever tried `schtasks /run`, which
    // fails with Access Denied from an unelevated logon context -- so it was
    // dead weight. Remove any such leftover from earlier versions.
    remove_legacy_startup_vbs();

    if enabled {
        let exe = std::env::current_exe()?;
        let target = format!(
            "\"{}\" --config \"{}\" --hidden",
            exe.display(),
            config_path.display()
        );
        let created = std::process::Command::new("schtasks")
            .args([
                "/create",
                "/tn",
                AUTOSTART_TASK_NAME,
                "/tr",
                &target,
                "/sc",
                "onlogon",
                "/rl",
                "highest",
                "/f",
            ])
            .status();
        match created {
            Ok(status) if status.success() => {}
            Ok(status) => warn!(code = ?status.code(), "schtasks /create returned non-zero"),
            Err(err) => warn!(error = %err, "failed to invoke schtasks /create"),
        }
    } else {
        let _ = std::process::Command::new("schtasks")
            .args(["/delete", "/tn", AUTOSTART_TASK_NAME, "/f"])
            .status();
    }
    Ok(())
}

/// Remove the obsolete Startup-folder launcher scripts that earlier versions
/// wrote. Best-effort; missing files are fine.
#[cfg(all(feature = "gui", windows))]
fn remove_legacy_startup_vbs() {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return;
    };
    let startup = PathBuf::from(appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup");
    for name in ["auto_sync-start.vbs", "auto_syncd-start.vbs"] {
        let _ = std::fs::remove_file(startup.join(name));
    }
}

#[cfg(all(feature = "gui", not(windows)))]
fn apply_autostart_inner(enabled: bool, config_path: &std::path::Path) -> std::io::Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(());
    };
    let dir = PathBuf::from(home).join(".config").join("autostart");
    let entry = dir.join("auto_sync.desktop");
    if enabled {
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()?;
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=auto_sync\nExec=\"{}\" --config \"{}\" --hidden\nX-GNOME-Autostart-enabled=true\n",
            exe.display(),
            config_path.display()
        );
        std::fs::write(&entry, content)?;
    } else if entry.exists() {
        std::fs::remove_file(&entry)?;
    }
    Ok(())
}

#[cfg(feature = "gui")]
fn show_main_window(app: &tauri::AppHandle) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(feature = "gui")]
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    let show = MenuItemBuilder::with_id("show", "Show auto_sync").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit auto_sync").build(app)?;
    let menu = MenuBuilder::new(app).items(&[&show, &quit]).build()?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../../icons/icon.png"))?;
    let _tray = TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("auto_sync")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

#[cfg(feature = "gui")]
fn run_with_desktop(backend: Backend, addr: SocketAddr, start_hidden: bool) {
    spawn_web(backend.clone(), addr);
    let config_path = backend.config_path();
    if let Ok(cfg) = backend.get_config() {
        CLOSE_TO_TRAY.store(cfg.app.close_to_tray, Ordering::Relaxed);
        apply_autostart(cfg.app.autostart, config_path.as_path());
    }
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(backend)
        .setup(move |app| {
            if let Err(err) = build_tray(app.handle()) {
                warn!(error = %err, "failed to create system tray");
            }
            // The window is created hidden (tauri.conf.json visible:false); show it
            // now unless we were launched into the tray (e.g. via autostart).
            if !start_hidden {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if CLOSE_TO_TRAY.load(Ordering::Relaxed) {
                    // Minimize to tray instead of quitting (daemon keeps running).
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    // Close means quit: stop the whole process (daemon included).
                    use tauri::Manager;
                    window.app_handle().exit(0);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_command,
            get_machines,
            discover_machines,
            add_machine,
            remove_machine,
            get_status,
            get_runtime_status,
            get_sync_activity,
            sync_now,
            sync_source_now,
            sync_destination_now,
            scan_destination_now,
            scan_report,
            browse_paths
        ])
        .run(tauri::generate_context!());
    if let Err(err) = result {
        error!(error = %err, "desktop app exited with error");
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
    record_startup_changes(&cfg, &state);

    let mut watcher_state = start_watcher(&cfg);
    let mut watcher_signature = config_signature(&cfg);
    let mut last_status_log =
        Instant::now() - Duration::from_secs(cfg.app.status_log_interval_secs);

    while !shutdown.load(Ordering::SeqCst) {
        match load_config(&config_path) {
            Ok(new_cfg) => {
                let signature = config_signature(&new_cfg);
                if signature != watcher_signature {
                    info!("config changed; restarting source watcher");
                    stop_watcher(&mut watcher_state);
                    record_startup_changes(&new_cfg, &state);
                    watcher_state = start_watcher(&new_cfg);
                    watcher_signature = signature;
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

fn record_startup_changes(cfg: &AppConfig, state: &State) {
    match record_startup_mtime_events(cfg, state) {
        Ok(recorded) if recorded > 0 => {
            info!(recorded, "startup mtime scan recorded realtime events");
        }
        Ok(_) => {}
        Err(err) => warn!(error = %err, "startup mtime scan failed"),
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
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_source_watcher_thread(cfg.clone(), cfg.app.data_db.clone(), stop.clone());
    WatcherState {
        stop,
        handle: Some(handle),
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

fn config_signature(cfg: &AppConfig) -> String {
    toml::to_string(cfg).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Desktop (Tauri) commands — compiled only with the `gui` feature
// ---------------------------------------------------------------------------

#[cfg(feature = "gui")]
fn error_text(err: anyhow::Error) -> String {
    err.to_string()
}

#[cfg(feature = "gui")]
#[tauri::command]
fn get_config(backend: tauri::State<'_, Backend>) -> Result<AppConfig, String> {
    backend.get_config().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn save_config_command(
    backend: tauri::State<'_, Backend>,
    cfg: AppConfig,
) -> Result<AppConfig, String> {
    let saved = backend.save_config(&cfg).map_err(error_text)?;
    CLOSE_TO_TRAY.store(saved.app.close_to_tray, Ordering::Relaxed);
    apply_autostart(saved.app.autostart, backend.config_path().as_path());
    Ok(saved)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn get_machines(backend: tauri::State<'_, Backend>) -> Result<MachineStatus, String> {
    backend.machines().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn discover_machines(backend: tauri::State<'_, Backend>) -> Result<MachineStatus, String> {
    backend.discover_machines().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn add_machine(
    backend: tauri::State<'_, Backend>,
    machine: MachineConfig,
) -> Result<AppConfig, String> {
    backend.add_machine(machine).map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn remove_machine(
    backend: tauri::State<'_, Backend>,
    machine_id: String,
) -> Result<AppConfig, String> {
    backend.remove_machine(&machine_id).map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn get_status(backend: tauri::State<'_, Backend>) -> Result<Vec<DestinationView>, String> {
    backend.status().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn get_runtime_status(backend: tauri::State<'_, Backend>) -> Result<RuntimeStatus, String> {
    Ok(backend.runtime_status())
}

#[cfg(feature = "gui")]
#[tauri::command]
fn get_sync_activity(backend: tauri::State<'_, Backend>) -> Result<SyncActivityStatus, String> {
    backend.sync_activity().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
async fn sync_now(backend: tauri::State<'_, Backend>) -> Result<Vec<DestinationView>, String> {
    backend.sync_now().map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
async fn sync_source_now(
    backend: tauri::State<'_, Backend>,
    source_id: String,
) -> Result<Vec<DestinationView>, String> {
    backend.sync_source_now(&source_id).map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
async fn sync_destination_now(
    backend: tauri::State<'_, Backend>,
    source_id: String,
    destination_id: String,
    mode: Option<String>,
) -> Result<Vec<DestinationView>, String> {
    let mode = mode
        .as_deref()
        .unwrap_or("incremental")
        .parse::<SyncRequestMode>()
        .map_err(error_text)?;
    backend
        .sync_destination_now(&source_id, &destination_id, mode)
        .map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
async fn scan_destination_now(
    backend: tauri::State<'_, Backend>,
    source_id: String,
    destination_id: String,
) -> Result<Option<ScanReport>, String> {
    backend
        .scan_destination_now(&source_id, &destination_id)
        .map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
async fn scan_report(
    backend: tauri::State<'_, Backend>,
    source_id: String,
    destination_id: String,
) -> Result<Option<ScanReport>, String> {
    backend
        .scan_report(&source_id, &destination_id)
        .map_err(error_text)
}

#[cfg(feature = "gui")]
#[tauri::command]
fn browse_paths(
    backend: tauri::State<'_, Backend>,
    path: Option<PathBuf>,
    machine_id: Option<String>,
) -> Result<BrowseResponse, String> {
    backend.browse_paths(path, machine_id).map_err(error_text)
}
