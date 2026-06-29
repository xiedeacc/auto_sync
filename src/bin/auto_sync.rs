#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

//! Unified `auto_sync` process: it always runs the scheduler + file watcher and
//! the web server in one process, and — on a desktop-capable build with a
//! display available — also opens the Tauri desktop window. Running everything
//! in a single process removes the old daemon/GUI contention over the shared
//! SQLite database and the destination machines.

use std::net::SocketAddr;
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
use auto_sync::core::watcher::spawn_source_watcher_thread;
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
use auto_sync::core::backend::{BrowseResponse, RuntimeStatus};
#[cfg(feature = "gui")]
use auto_sync::core::config::MachineConfig;
#[cfg(feature = "gui")]
use auto_sync::core::machines::MachineStatus;
#[cfg(feature = "gui")]
use auto_sync::core::state::DestinationView;
#[cfg(feature = "gui")]
use auto_sync::core::sync::SyncRequestMode;

#[derive(Debug, Parser)]
#[command(name = "auto_sync")]
#[command(about = "auto_sync — directory sync daemon, web UI, and optional desktop app")]
struct Args {
    #[arg(long, default_value = "conf/auto_sync.toml")]
    config: PathBuf,
    /// Run web-only, never opening the desktop window even on a GUI build.
    #[arg(long)]
    no_gui: bool,
    /// Internal guard to avoid repeated UAC relaunch attempts.
    #[cfg(windows)]
    #[arg(long, hide = true)]
    elevation_attempted: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = load_or_create_config(&args.config)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync")?;
    info!(config = %args.config.display(), "auto_sync starting");
    #[cfg(windows)]
    if maybe_relaunch_elevated_on_windows(&args)? {
        return Ok(());
    }

    // Apply receiver-side policy up front so the web server (the destination of
    // pushes) honours it even though it never runs the scheduler loop.
    auto_sync::core::sync::configure_fsync(cfg.app.sync.fsync);

    let config_path = args
        .config
        .canonicalize()
        .unwrap_or_else(|_| args.config.clone());
    let addr: SocketAddr = cfg
        .app
        .web_bind
        .parse()
        .with_context(|| format!("invalid bind address {}", cfg.app.web_bind))?;

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

#[cfg(windows)]
fn maybe_relaunch_elevated_on_windows(args: &Args) -> Result<bool> {
    if args.elevation_attempted || unsafe { IsUserAnAdmin() } != 0 {
        return Ok(false);
    }
    warn!("auto_sync is not elevated on Windows; relaunching with UAC");

    let exe = std::env::current_exe().context("failed to locate current executable")?;
    let working_dir = std::env::current_dir().context("failed to locate current directory")?;
    let config = args
        .config
        .canonicalize()
        .unwrap_or_else(|_| args.config.clone());
    let mut relaunch_args = vec![
        OsString::from("--config"),
        config.into_os_string(),
        OsString::from("--elevation-attempted"),
    ];
    if args.no_gui {
        relaunch_args.push(OsString::from("--no-gui"));
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
            run_with_desktop(backend, addr);
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
fn run_with_desktop(backend: Backend, addr: SocketAddr) {
    spawn_web(backend.clone(), addr);
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(backend)
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_command,
            get_machines,
            discover_machines,
            add_machine,
            remove_machine,
            get_status,
            get_runtime_status,
            sync_now,
            sync_source_now,
            sync_destination_now,
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

        thread::sleep(Duration::from_secs(5));
    }

    stop_watcher(&mut watcher_state);
    Ok(())
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
    backend.save_config(&cfg).map_err(error_text)
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
fn browse_paths(
    backend: tauri::State<'_, Backend>,
    path: Option<PathBuf>,
    machine_id: Option<String>,
) -> Result<BrowseResponse, String> {
    backend.browse_paths(path, machine_id).map_err(error_text)
}
