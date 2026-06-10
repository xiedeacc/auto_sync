use std::path::PathBuf;

use anyhow::{Context, Result};
use auto_sync::core::config::{AppConfig, load_config, load_or_create_config, save_config};
use auto_sync::core::logging::init_logging;
use auto_sync::core::state::{DestinationView, State as DbState};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "auto_sync_gui")]
#[command(about = "Tauri Linux GUI for auto_sync")]
struct Args {
    #[arg(long, default_value = "conf/auto_sync.toml")]
    config: PathBuf,
}

#[derive(Clone)]
struct GuiState {
    config_path: PathBuf,
}

#[tauri::command]
fn get_config(state: tauri::State<'_, GuiState>) -> Result<AppConfig, String> {
    load_or_create_config(&state.config_path).map_err(error_text)
}

#[tauri::command]
fn save_config_command(
    state: tauri::State<'_, GuiState>,
    cfg: AppConfig,
) -> Result<AppConfig, String> {
    save_config(&state.config_path, &cfg).map_err(error_text)?;
    let state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    Ok(cfg)
}

#[tauri::command]
fn get_status(state: tauri::State<'_, GuiState>) -> Result<Vec<DestinationView>, String> {
    let cfg = load_config(&state.config_path).map_err(error_text)?;
    let state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    state_db.destination_views(&cfg).map_err(error_text)
}

#[tauri::command]
async fn sync_now(_state: tauri::State<'_, GuiState>) -> Result<Vec<DestinationView>, String> {
    Err("sync is disabled during UI development".to_string())
}

#[tauri::command]
async fn sync_source_now(
    _state: tauri::State<'_, GuiState>,
    _source_id: String,
) -> Result<Vec<DestinationView>, String> {
    Err("sync is disabled during UI development".to_string())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = load_or_create_config(&args.config)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync_gui")?;
    let state = GuiState {
        config_path: args
            .config
            .canonicalize()
            .unwrap_or_else(|_| args.config.clone()),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_command,
            get_status,
            sync_now,
            sync_source_now
        ])
        .run(tauri::generate_context!())
        .context("failed to run Tauri GUI")?;
    Ok(())
}

fn error_text(err: anyhow::Error) -> String {
    err.to_string()
}
