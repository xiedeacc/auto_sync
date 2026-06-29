use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State as AxumState;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tracing::info;

use crate::core::backend::{Backend, BrowseResponse, RuntimeStatus};
use crate::core::config::{AppConfig, MachineConfig};
use crate::core::machines::{MachineHealth, MachineStatus, spawn_discovery_responder};
use crate::core::state::{DestinationView, SnapshotEntry};
use crate::core::sync::{
    SyncRequestMode, TransferAck, TransferApplyDeltaQuery, TransferBlockSumsRequest,
    TransferCleanupTmpRequest, TransferPathInfo, TransferPathInfoRequest,
    TransferPrepareDirRequest, TransferPrepareDirsRequest, TransferPushFileRequest,
    TransferPutFileQuery, TransferReceiveFileChunkQuery, TransferReceiveSymlinkRequest,
    TransferRemovePathRequest, TransferRemovePathsRequest, TransferSnapshotRequest,
    transfer_apply_delta, transfer_block_sums, transfer_cleanup_tmp, transfer_file_offset,
    transfer_finish_file, transfer_path_info, transfer_prepare_dir, transfer_prepare_dirs,
    transfer_push_file, transfer_put_file, transfer_receive_file_chunk, transfer_receive_symlink,
    transfer_remove_path, transfer_remove_paths, transfer_snapshot,
};

pub fn router(backend: Backend) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/main.js", get(main_js))
        .route("/styles.css", get(styles_css))
        .route("/api/config", get(api_get_config).post(api_save_config))
        .route("/api/health", get(api_health))
        .route("/api/machines", get(api_machines).post(api_add_machine))
        .route("/api/machines/:machine_id", delete(api_remove_machine))
        .route("/api/machines/discover", get(api_discover_machines))
        .route("/api/status", get(api_status))
        .route("/api/runtime-status", get(api_runtime_status))
        .route("/api/sync-now", post(api_sync_now))
        .route("/api/sync-source-now", post(api_sync_source_now))
        .route("/api/sync-destination-now", post(api_sync_destination_now))
        .route("/api/transfer/snapshot", post(api_transfer_snapshot))
        .route("/api/transfer/path-info", post(api_transfer_path_info))
        .route("/api/transfer/prepare-dir", post(api_transfer_prepare_dir))
        .route(
            "/api/transfer/prepare-dirs",
            post(api_transfer_prepare_dirs),
        )
        .route("/api/transfer/remove-path", post(api_transfer_remove_path))
        .route(
            "/api/transfer/remove-paths",
            post(api_transfer_remove_paths),
        )
        .route("/api/transfer/cleanup-tmp", post(api_transfer_cleanup_tmp))
        .route("/api/transfer/file-offset", post(api_transfer_file_offset))
        .route("/api/transfer/put-file", post(api_transfer_put_file))
        .route("/api/transfer/block-sums", post(api_transfer_block_sums))
        .route("/api/transfer/apply-delta", post(api_transfer_apply_delta))
        .route(
            "/api/transfer/receive-file-chunk",
            post(api_transfer_receive_file_chunk),
        )
        .route("/api/transfer/finish-file", post(api_transfer_finish_file))
        .route(
            "/api/transfer/receive-symlink",
            post(api_transfer_receive_symlink),
        )
        .route("/api/transfer/push-file", post(api_transfer_push_file))
        .route("/api/browse-dirs", get(api_browse_paths))
        .route("/api/browse-paths", get(api_browse_paths))
        .layer(DefaultBodyLimit::disable())
        .with_state(backend)
}

pub async fn serve(backend: Backend, addr: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let _discovery = spawn_discovery_responder(backend.config_path(), backend.web_port());
    let app = router(backend);
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
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<AppConfig>, ApiError> {
    Ok(Json(backend.get_config()?))
}

async fn api_save_config(
    AxumState(backend): AxumState<Backend>,
    Json(cfg): Json<AppConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    Ok(Json(backend.save_config(&cfg)?))
}

async fn api_health(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineHealth>, ApiError> {
    Ok(Json(backend.health()?))
}

async fn api_machines(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineStatus>, ApiError> {
    Ok(Json(backend.machines()?))
}

async fn api_discover_machines(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineStatus>, ApiError> {
    Ok(Json(backend.discover_machines()?))
}

async fn api_add_machine(
    AxumState(backend): AxumState<Backend>,
    Json(machine): Json<MachineConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    Ok(Json(backend.add_machine(machine)?))
}

async fn api_remove_machine(
    AxumState(backend): AxumState<Backend>,
    AxumPath(machine_id): AxumPath<String>,
) -> Result<Json<AppConfig>, ApiError> {
    Ok(Json(backend.remove_machine(&machine_id)?))
}

async fn api_status(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    Ok(Json(backend.status()?))
}

async fn api_runtime_status(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<RuntimeStatus>, ApiError> {
    Ok(Json(backend.runtime_status()))
}

async fn api_sync_now(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    Ok(Json(backend.sync_now()?))
}

#[derive(Debug, Deserialize)]
struct SyncSourceRequest {
    source_id: String,
}

async fn api_sync_source_now(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<SyncSourceRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    Ok(Json(backend.sync_source_now(&req.source_id)?))
}

#[derive(Debug, Deserialize)]
struct SyncDestinationRequest {
    source_id: String,
    destination_id: String,
    mode: Option<String>,
}

async fn api_sync_destination_now(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<SyncDestinationRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    let mode = req
        .mode
        .as_deref()
        .unwrap_or("incremental")
        .parse::<SyncRequestMode>()?;
    Ok(Json(backend.sync_destination_now(
        &req.source_id,
        &req.destination_id,
        mode,
    )?))
}

async fn api_transfer_snapshot(
    Json(req): Json<TransferSnapshotRequest>,
) -> Result<Json<Vec<SnapshotEntry>>, ApiError> {
    Ok(Json(transfer_snapshot(req)?))
}

async fn api_transfer_path_info(
    Json(req): Json<TransferPathInfoRequest>,
) -> Result<Json<TransferPathInfo>, ApiError> {
    Ok(Json(transfer_path_info(req)?))
}

async fn api_transfer_prepare_dir(
    Json(req): Json<TransferPrepareDirRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_prepare_dir(req)?))
}

async fn api_transfer_prepare_dirs(
    Json(req): Json<TransferPrepareDirsRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_prepare_dirs(req)?))
}

async fn api_transfer_remove_path(
    Json(req): Json<TransferRemovePathRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_remove_path(req)?))
}

async fn api_transfer_remove_paths(
    Json(req): Json<TransferRemovePathsRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_remove_paths(req)?))
}

async fn api_transfer_cleanup_tmp(
    Json(req): Json<TransferCleanupTmpRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_cleanup_tmp(req)?))
}

async fn api_transfer_put_file(
    Query(query): Query<TransferPutFileQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_put_file(query, &body)?))
}

async fn api_transfer_block_sums(
    Json(req): Json<TransferBlockSumsRequest>,
) -> Result<Json<crate::core::sync::delta::BlockSums>, ApiError> {
    Ok(Json(transfer_block_sums(req)?))
}

async fn api_transfer_apply_delta(
    Query(query): Query<TransferApplyDeltaQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_apply_delta(query, &body)?))
}

async fn api_transfer_file_offset(
    Json(req): Json<crate::core::sync::TransferFileOffsetRequest>,
) -> Result<Json<crate::core::sync::TransferFileOffset>, ApiError> {
    Ok(Json(transfer_file_offset(req)?))
}

async fn api_transfer_receive_file_chunk(
    Query(query): Query<TransferReceiveFileChunkQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_receive_file_chunk(query, &body)?))
}

async fn api_transfer_finish_file(
    Json(req): Json<crate::core::sync::TransferFinishFileRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_finish_file(req)?))
}

async fn api_transfer_receive_symlink(
    Json(req): Json<TransferReceiveSymlinkRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_receive_symlink(req)?))
}

async fn api_transfer_push_file(
    Json(req): Json<TransferPushFileRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    Ok(Json(transfer_push_file(req)?))
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    path: Option<PathBuf>,
    machine_id: Option<String>,
}

async fn api_browse_paths(
    AxumState(backend): AxumState<Backend>,
    Query(query): Query<BrowseQuery>,
) -> Result<Json<BrowseResponse>, ApiError> {
    Ok(Json(backend.browse_paths(query.path, query.machine_id)?))
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
