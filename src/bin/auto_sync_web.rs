use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use auto_sync::core::config::{AppConfig, load_config, load_or_create_config, save_config};
use auto_sync::core::logging::init_logging;
use auto_sync::core::state::{DestinationView, State as DbState};
use axum::extract::Query;
use axum::extract::State as AxumState;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
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
    };
    let app = Router::new()
        .route("/", get(index))
        .route("/main.js", get(main_js))
        .route("/styles.css", get(styles_css))
        .route("/api/config", get(api_get_config).post(api_save_config))
        .route("/api/status", get(api_status))
        .route("/api/sync-now", post(api_sync_now))
        .route("/api/sync-source-now", post(api_sync_source_now))
        .route("/api/browse-dirs", get(api_browse_dirs))
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
    save_config(&state.config_path, &cfg)?;
    let state_db = DbState::open(&cfg.app.data_db)?;
    state_db.ensure_config(&cfg)?;
    Ok(Json(cfg))
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
    AxumState(_state): AxumState<WebState>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    Err(ApiError(anyhow::anyhow!(
        "sync is disabled during UI development"
    )))
}

#[derive(Debug, Deserialize)]
struct SyncSourceRequest {
    source_id: String,
}

async fn api_sync_source_now(
    AxumState(_state): AxumState<WebState>,
    Json(req): Json<SyncSourceRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    Err(ApiError(anyhow::anyhow!(
        "sync is disabled during UI development for source {}",
        req.source_id
    )))
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    path: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct BrowseEntry {
    name: String,
    path: String,
}

#[derive(Debug, Serialize)]
struct BrowseResponse {
    path: String,
    parent: Option<String>,
    entries: Vec<BrowseEntry>,
}

async fn api_browse_dirs(
    Query(query): Query<BrowseQuery>,
) -> Result<Json<BrowseResponse>, ApiError> {
    let requested = query.path.unwrap_or_else(|| PathBuf::from("/"));
    let path = normalize_dir(&requested)?;
    let parent = path
        .parent()
        .map(|parent| parent.to_string_lossy().to_string());
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if !metadata.is_dir() {
            continue;
        }
        let entry_path = entry.path();
        entries.push(BrowseEntry {
            name: entry.file_name().to_string_lossy().to_string(),
            path: entry_path.to_string_lossy().to_string(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(Json(BrowseResponse {
        path: path.to_string_lossy().to_string(),
        parent,
        entries,
    }))
}

fn normalize_dir(path: &Path) -> Result<PathBuf> {
    let path = if path.as_os_str().is_empty() {
        Path::new("/")
    } else {
        path
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to open directory {}", path.display()))?;
    if !canonical.is_dir() {
        anyhow::bail!("not a directory: {}", canonical.display());
    }
    Ok(canonical)
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
