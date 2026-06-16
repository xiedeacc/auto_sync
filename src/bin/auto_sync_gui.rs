#![cfg_attr(windows, windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use auto_sync::core::config::{
    AppConfig, MachineConfig, load_config, load_or_create_config, save_config,
};
use auto_sync::core::logging::init_logging;
use auto_sync::core::machines::{
    MachineStatus, discover_lan, encode_query_component, find_machine, machine_id_from_path,
    machine_status, remote_get_json, spawn_discovery_responder,
};
use auto_sync::core::state::{DestinationView, State as DbState};
use auto_sync::core::sync::{
    sync_all_now, sync_destination_now as sync_destination_now_core,
    sync_source_now as sync_source_now_core,
};
use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(name = "auto_sync_gui")]
#[command(about = "Tauri GUI for auto_sync")]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
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
    let cfg = save_config(&state.config_path, &cfg).map_err(error_text)?;
    let state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    Ok(cfg)
}

#[tauri::command]
fn get_machines(state: tauri::State<'_, GuiState>) -> Result<MachineStatus, String> {
    let cfg = load_or_create_config(&state.config_path).map_err(error_text)?;
    Ok(machine_status(&cfg))
}

#[tauri::command]
fn discover_machines(state: tauri::State<'_, GuiState>) -> Result<MachineStatus, String> {
    let cfg = load_or_create_config(&state.config_path).map_err(error_text)?;
    let discovered = discover_lan(std::time::Duration::from_millis(700)).map_err(error_text)?;
    Ok(auto_sync::core::machines::merge_discovered(
        &cfg, discovered,
    ))
}

#[tauri::command]
fn add_machine(
    state: tauri::State<'_, GuiState>,
    machine: MachineConfig,
) -> Result<AppConfig, String> {
    let mut cfg = load_or_create_config(&state.config_path).map_err(error_text)?;
    if let Some(existing) = cfg.machines.iter_mut().find(|item| item.id == machine.id) {
        *existing = machine;
    } else {
        cfg.machines.push(machine);
    }
    save_config(&state.config_path, &cfg).map_err(error_text)
}

#[tauri::command]
fn get_status(state: tauri::State<'_, GuiState>) -> Result<Vec<DestinationView>, String> {
    let cfg = load_config(&state.config_path).map_err(error_text)?;
    let state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    state_db.destination_views(&cfg).map_err(error_text)
}

#[tauri::command]
async fn sync_now(state: tauri::State<'_, GuiState>) -> Result<Vec<DestinationView>, String> {
    let cfg = load_config(&state.config_path).map_err(error_text)?;
    let mut state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    sync_all_now(&cfg, &mut state_db).map_err(error_text)?;
    state_db.destination_views(&cfg).map_err(error_text)
}

#[tauri::command]
async fn sync_source_now(
    state: tauri::State<'_, GuiState>,
    source_id: String,
) -> Result<Vec<DestinationView>, String> {
    let cfg = load_config(&state.config_path).map_err(error_text)?;
    let mut state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    sync_source_now_core(&cfg, &mut state_db, &source_id).map_err(error_text)?;
    state_db.destination_views(&cfg).map_err(error_text)
}

#[tauri::command]
async fn sync_destination_now(
    state: tauri::State<'_, GuiState>,
    source_id: String,
    destination_id: String,
) -> Result<Vec<DestinationView>, String> {
    let cfg = load_config(&state.config_path).map_err(error_text)?;
    let mut state_db = DbState::open(&cfg.app.data_db).map_err(error_text)?;
    state_db.ensure_config(&cfg).map_err(error_text)?;
    sync_destination_now_core(&cfg, &mut state_db, &source_id, &destination_id)
        .map_err(error_text)?;
    state_db.destination_views(&cfg).map_err(error_text)
}

#[derive(Debug, Serialize, Deserialize)]
struct BrowseEntry {
    name: String,
    path: String,
    kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BrowseResponse {
    path: String,
    parent: Option<String>,
    entries: Vec<BrowseEntry>,
}

#[tauri::command]
fn browse_paths(
    state: tauri::State<'_, GuiState>,
    path: Option<PathBuf>,
    machine_id: Option<String>,
) -> Result<BrowseResponse, String> {
    let machine_id = machine_id_from_path(machine_id.as_deref());
    if machine_id != "local" {
        let cfg = load_config(&state.config_path).map_err(error_text)?;
        let machine = find_machine(&cfg, machine_id)
            .ok_or_else(|| format!("unknown machine: {machine_id}"))?;
        let requested = path.unwrap_or_else(|| default_path_for_os(&machine.os));
        let path = format!(
            "/api/browse-paths?path={}",
            encode_query_component(&requested.to_string_lossy())
        );
        return remote_get_json::<BrowseResponse>(
            &machine,
            &path,
            std::time::Duration::from_secs(3),
        )
        .map_err(error_text);
    }
    browse_paths_inner(path.unwrap_or_else(|| PathBuf::from("/"))).map_err(error_text)
}

fn default_path_for_os(os: &str) -> PathBuf {
    if os.eq_ignore_ascii_case("windows") {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    }
}

fn browse_paths_inner(requested: PathBuf) -> Result<BrowseResponse> {
    let path = normalize_browse_dir(&requested)?;
    let parent = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string());
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        let kind = if metadata.is_dir() {
            "dir"
        } else if metadata.is_file() {
            "file"
        } else {
            continue;
        };
        let entry_path = entry.path();
        entries.push(BrowseEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: entry_path.to_string_lossy().to_string(),
            kind: kind.to_string(),
        });
    }
    entries.sort_by(|left, right| {
        entry_kind_order(&left.kind)
            .cmp(&entry_kind_order(&right.kind))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(BrowseResponse {
        path: path.to_string_lossy().to_string(),
        parent,
        entries,
    })
}

fn normalize_browse_dir(path: &Path) -> Result<PathBuf> {
    let path = if path.as_os_str().is_empty() {
        Path::new("/")
    } else {
        path
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to open path {}", path.display()))?;
    if canonical.is_file() {
        return canonical
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", canonical.display()));
    }
    if !canonical.is_dir() {
        anyhow::bail!("not a browsable path: {}", canonical.display());
    }
    Ok(canonical)
}

fn entry_kind_order(kind: &str) -> u8 {
    if kind == "dir" { 0 } else { 1 }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let config_arg = args.config.unwrap_or_else(default_gui_config_path);
    let cfg = load_or_create_config(&config_arg)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync_gui")?;
    let config_path = config_arg
        .canonicalize()
        .unwrap_or_else(|_| config_arg.clone());
    let web_port = cfg
        .app
        .web_bind
        .parse::<SocketAddr>()
        .map(|addr| addr.port())
        .unwrap_or(18765);
    let _discovery = spawn_discovery_responder(Arc::new(config_path.clone()), web_port);
    let state = GuiState { config_path };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config_command,
            get_machines,
            discover_machines,
            add_machine,
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
