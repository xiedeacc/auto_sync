use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use auto_sync::core::config::{
    AppConfig, MachineConfig, load_config, load_or_create_config, save_config,
};
use auto_sync::core::logging::init_logging;
use auto_sync::core::machines::{
    MachineHealth, MachineStatus, discover_lan, encode_query_component, find_machine, local_health,
    machine_id_from_path, merge_discovered, remote_get_json, spawn_discovery_responder,
};
use auto_sync::core::state::{DestinationView, State as DbState};
use auto_sync::core::sync::{sync_all_now, sync_destination_now, sync_source_now};
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State as AxumState;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "auto_sync_web")]
#[command(about = "Headless Web UI for auto_sync")]
struct Args {
    #[arg(long, default_value = "conf/auto_sync.toml")]
    config: PathBuf,

    #[arg(long)]
    bind: Option<String>,
}

#[derive(Clone)]
struct WebState {
    config_path: Arc<PathBuf>,
    web_port: u16,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let cfg = load_or_create_config(&args.config)?;
    let _log_guard = init_logging(&cfg.app.log_dir, "auto_sync_web")?;
    let bind = args.bind.unwrap_or_else(|| cfg.app.web_bind.clone());
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("invalid bind address {bind}"))?;

    let state = WebState {
        config_path: Arc::new(args.config),
        web_port: addr.port(),
    };
    let _discovery = spawn_discovery_responder(state.config_path.clone(), addr.port());
    let app = Router::new()
        .route("/", get(index))
        .route("/main.js", get(main_js))
        .route("/styles.css", get(styles_css))
        .route("/api/config", get(api_get_config).post(api_save_config))
        .route("/api/health", get(api_health))
        .route("/api/machines", get(api_machines).post(api_add_machine))
        .route("/api/machines/:machine_id", delete(api_remove_machine))
        .route("/api/machines/discover", get(api_discover_machines))
        .route("/api/status", get(api_status))
        .route("/api/sync-now", post(api_sync_now))
        .route("/api/sync-source-now", post(api_sync_source_now))
        .route("/api/sync-destination-now", post(api_sync_destination_now))
        .route("/api/browse-dirs", get(api_browse_paths))
        .route("/api/browse-paths", get(api_browse_paths))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(url = %format!("http://{addr}/"), "auto_sync web listening");
    println!("auto_sync Web UI: http://{addr}/");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../ui/index.html"))
}

async fn main_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        include_str!("../ui/main.js"),
    )
        .into_response()
}

async fn styles_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css")],
        include_str!("../ui/styles.css"),
    )
        .into_response()
}

async fn api_get_config(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<AppConfig>, ApiError> {
    Ok(Json(load_or_create_config(&state.config_path)?))
}

async fn api_save_config(
    AxumState(state): AxumState<WebState>,
    Json(cfg): Json<AppConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    let cfg = save_config(&state.config_path, &cfg)?;
    let state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    Ok(Json(cfg))
}

async fn api_health(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<MachineHealth>, ApiError> {
    let cfg = load_or_create_config(&state.config_path)?;
    Ok(Json(local_health(&cfg, state.web_port)))
}

async fn api_machines(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<MachineStatus>, ApiError> {
    let cfg = load_or_create_config(&state.config_path)?;
    Ok(Json(auto_sync::core::machines::machine_status(&cfg)))
}

async fn api_discover_machines(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<MachineStatus>, ApiError> {
    let cfg = load_or_create_config(&state.config_path)?;
    let discovered = discover_lan(std::time::Duration::from_millis(700))?;
    Ok(Json(merge_discovered(&cfg, discovered)))
}

async fn api_add_machine(
    AxumState(state): AxumState<WebState>,
    Json(machine): Json<MachineConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    let mut cfg = load_or_create_config(&state.config_path)?;
    if let Some(existing) = cfg.machines.iter_mut().find(|item| item.id == machine.id) {
        *existing = machine;
    } else {
        cfg.machines.push(machine);
    }
    Ok(Json(save_config(&state.config_path, &cfg)?))
}

async fn api_remove_machine(
    AxumState(state): AxumState<WebState>,
    AxumPath(machine_id): AxumPath<String>,
) -> Result<Json<AppConfig>, ApiError> {
    if machine_id == "local" {
        return Err(anyhow::anyhow!("local machine cannot be deleted").into());
    }
    let mut cfg = load_or_create_config(&state.config_path)?;
    cfg.machines.retain(|machine| machine.id != machine_id);
    Ok(Json(save_config(&state.config_path, &cfg)?))
}

async fn api_status(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    let cfg = load_config(&state.config_path)?;
    let state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    Ok(Json(state_db.destination_views(&cfg)?))
}

async fn api_sync_now(
    AxumState(state): AxumState<WebState>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    let cfg = load_config(&state.config_path)?;
    let mut state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    sync_all_now(&cfg, &mut state_db)?;
    Ok(Json(state_db.destination_views(&cfg)?))
}

#[derive(Debug, Deserialize)]
struct SyncSourceRequest {
    source_id: String,
}

async fn api_sync_source_now(
    AxumState(state): AxumState<WebState>,
    Json(req): Json<SyncSourceRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    let cfg = load_config(&state.config_path)?;
    let mut state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    sync_source_now(&cfg, &mut state_db, &req.source_id)?;
    Ok(Json(state_db.destination_views(&cfg)?))
}

#[derive(Debug, Deserialize)]
struct SyncDestinationRequest {
    source_id: String,
    destination_id: String,
}

async fn api_sync_destination_now(
    AxumState(state): AxumState<WebState>,
    Json(req): Json<SyncDestinationRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    let cfg = load_config(&state.config_path)?;
    let mut state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    sync_destination_now(&cfg, &mut state_db, &req.source_id, &req.destination_id)?;
    Ok(Json(state_db.destination_views(&cfg)?))
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    path: Option<PathBuf>,
    machine_id: Option<String>,
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

async fn api_browse_paths(
    AxumState(state): AxumState<WebState>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<BrowseResponse>, ApiError> {
    let machine_id = machine_id_from_path(query.machine_id.as_deref());
    if machine_id != "local" {
        let cfg = load_config(&state.config_path)?;
        let machine = find_machine(&cfg, machine_id)
            .ok_or_else(|| anyhow::anyhow!("unknown machine: {machine_id}"))?;
        let requested = query
            .path
            .unwrap_or_else(|| default_path_for_os(&machine.os));
        let path = format!(
            "/api/browse-paths?path={}",
            encode_query_component(&requested.to_string_lossy())
        );
        let response =
            remote_get_json::<BrowseResponse>(&machine, &path, std::time::Duration::from_secs(3))?;
        return Ok(Json(response));
    }
    let requested = query.path.unwrap_or_else(|| PathBuf::from("/"));
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
    Ok(Json(BrowseResponse {
        path: path.to_string_lossy().to_string(),
        parent,
        entries,
    }))
}

fn default_path_for_os(os: &str) -> PathBuf {
    if os.eq_ignore_ascii_case("windows") {
        PathBuf::from("C:\\")
    } else {
        PathBuf::from("/")
    }
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

struct ApiError(anyhow::Error);

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}
