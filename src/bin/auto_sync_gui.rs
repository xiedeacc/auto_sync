#![cfg_attr(windows, windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use auto_sync::core::backend::{Backend, BrowseResponse};
use auto_sync::core::config::{AppConfig, MachineConfig, load_or_create_config};
use auto_sync::core::logging::init_logging;
use auto_sync::core::machines::MachineStatus;
use auto_sync::core::state::DestinationView;
use auto_sync::core::web_api;
use clap::Parser;
use tracing::warn;

#[derive(Debug, Parser)]
#[command(name = "auto_sync_gui")]
#[command(about = "Tauri GUI for auto_sync")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
}

#[tauri::command]
fn get_config(backend: tauri::State<'_, Backend>) -> Result<AppConfig, String> {
    backend.get_config().map_err(error_text)
}

#[tauri::command]
fn save_config_command(
    backend: tauri::State<'_, Backend>,
    cfg: AppConfig,
) -> Result<AppConfig, String> {
    backend.save_config(&cfg).map_err(error_text)
}

#[tauri::command]
fn get_machines(backend: tauri::State<'_, Backend>) -> Result<MachineStatus, String> {
    backend.machines().map_err(error_text)
}

#[tauri::command]
fn discover_machines(backend: tauri::State<'_, Backend>) -> Result<MachineStatus, String> {
    backend.discover_machines().map_err(error_text)
}

#[tauri::command]
fn add_machine(
    backend: tauri::State<'_, Backend>,
    machine: MachineConfig,
) -> Result<AppConfig, String> {
    backend.add_machine(machine).map_err(error_text)
}

#[tauri::command]
fn remove_machine(
    backend: tauri::State<'_, Backend>,
    machine_id: String,
) -> Result<AppConfig, String> {
    backend.remove_machine(&machine_id).map_err(error_text)
}

#[tauri::command]
fn get_status(backend: tauri::State<'_, Backend>) -> Result<Vec<DestinationView>, String> {
    backend.status().map_err(error_text)
}

#[tauri::command]
async fn sync_now(backend: tauri::State<'_, Backend>) -> Result<Vec<DestinationView>, String> {
    backend.sync_now().map_err(error_text)
}

#[tauri::command]
async fn sync_source_now(
    backend: tauri::State<'_, Backend>,
    source_id: String,
) -> Result<Vec<DestinationView>, String> {
    backend.sync_source_now(&source_id).map_err(error_text)
}

#[tauri::command]
async fn sync_destination_now(
    backend: tauri::State<'_, Backend>,
    source_id: String,
    destination_id: String,
) -> Result<Vec<DestinationView>, String> {
    backend
        .sync_destination_now(&source_id, &destination_id)
        .map_err(error_text)
}

#[tauri::command]
fn browse_paths(
    backend: tauri::State<'_, Backend>,
    path: Option<PathBuf>,
    machine_id: Option<String>,
) -> Result<BrowseResponse, String> {
    backend.browse_paths(path, machine_id).map_err(error_text)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_arg = args.config.unwrap_or_else(default_gui_config_path);
    let cfg = load_or_create_config(&config_arg)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync_gui")?;
    let config_path = config_arg
        .canonicalize()
        .unwrap_or_else(|_| config_arg.clone());
    let addr: SocketAddr = cfg
        .app
        .web_bind
        .parse()
        .with_context(|| format!("invalid bind address {}", cfg.app.web_bind))?;
    let backend = Backend::new(config_path, addr.port());
    spawn_embedded_web_backend(backend.clone(), addr);

    tauri::Builder::default()
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
            sync_now,
            sync_source_now,
            sync_destination_now,
            browse_paths
        ])
        .run(tauri::generate_context!())
        .context("failed to run Tauri GUI")?;
    Ok(())
}

fn spawn_embedded_web_backend(backend: Backend, addr: SocketAddr) {
    let result = std::thread::Builder::new()
        .name("auto_sync_gui_web_backend".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    warn!(error = %err, "failed to create GUI web backend runtime");
                    return;
                }
            };
            if let Err(err) = runtime.block_on(web_api::serve(backend, addr)) {
                warn!(error = %err, %addr, "GUI web backend stopped");
            }
        });
    if let Err(err) = result {
        warn!(error = %err, "failed to spawn GUI web backend");
    }
}

fn default_gui_config_path() -> PathBuf {
    let Ok(exe) = std::env::current_exe() else {
        return PathBuf::from("conf/auto_sync.toml");
    };
    let Some(exe_dir) = exe.parent() else {
        return PathBuf::from("conf/auto_sync.toml");
    };
    let install_dir = if exe_dir
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("bin"))
    {
        exe_dir.parent().unwrap_or(exe_dir)
    } else {
        exe_dir
    };
    install_dir.join("conf").join("auto_sync.toml")
}

fn error_text(err: anyhow::Error) -> String {
    err.to_string()
}
