use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use axum::extract::Path as AxumPath;
use axum::extract::Query;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tracing::info;

use crate::core::backend::{
    Backend, BrowseResponse, CancelActivityRequest, CancelOutcome, DelegatedSourceGroupsRequest,
    RuntimeStatus, SyncActivityStatus,
};
use crate::core::config::{AppConfig, MachineConfig};
use crate::core::machines::{MachineHealth, MachineStatus, spawn_discovery_responder};
use crate::core::state::{DestinationView, ScanReport, SnapshotEntry};
use crate::core::sync::{
    SyncRequestMode, TransferAck, TransferApplyDeltaQuery, TransferBlockSumsRequest,
    TransferCleanupTmpRequest, TransferPathInfo, TransferPathInfoRequest,
    TransferPrepareDirRequest, TransferPrepareDirsRequest, TransferPushFileRequest,
    TransferPutFileQuery, TransferReceiveFileChunkQuery, TransferReceiveSymlinkRequest,
    TransferRemovePathRequest, TransferRemovePathsRequest, TransferSetDirMtimesRequest,
    TransferSetModesRequest, TransferSnapshotPathsRequest, TransferSnapshotRequest,
    transfer_apply_delta, transfer_block_sums, transfer_cleanup_tmp, transfer_file_offset,
    transfer_finish_file, transfer_path_info, transfer_prepare_dir, transfer_prepare_dirs,
    transfer_push_file, transfer_put_file, transfer_receive_file_chunk, transfer_receive_symlink,
    transfer_remove_path, transfer_remove_paths, transfer_set_dir_mtimes, transfer_set_modes,
    transfer_snapshot, transfer_snapshot_paths,
};

/// Peer-only surface: these endpoints write/delete under destination roots
/// or replace delegated config, and are never called by the browser UI. With
/// `app.peer_token` set (same value on every machine) they require the
/// matching header; empty keeps the open LAN-trust behavior.
fn path_requires_peer_token(path: &str) -> bool {
    path.starts_with("/api/transfer/") || path == "/api/config/delegated-source-groups"
}

async fn require_peer_token(req: Request<axum::body::Body>, next: Next) -> Response {
    let expected = crate::core::machines::peer_token();
    if !expected.is_empty() && path_requires_peer_token(req.uri().path()) {
        let provided = req
            .headers()
            .get(crate::core::machines::PEER_TOKEN_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if provided != expected {
            return (StatusCode::UNAUTHORIZED, "peer token missing or wrong").into_response();
        }
    }
    next.run(req).await
}

pub fn router(backend: Backend) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/main.js", get(main_js))
        .route("/styles.css", get(styles_css))
        .route("/api/config", get(api_get_config).post(api_save_config))
        .route(
            "/api/config/delegated-source-groups",
            post(api_delegated_source_groups),
        )
        .route("/api/health", get(api_health))
        .route("/api/machines", get(api_machines).post(api_add_machine))
        .route("/api/machines/:machine_id", delete(api_remove_machine))
        .route("/api/machines/discover", get(api_discover_machines))
        .route("/api/status", get(api_status))
        .route("/api/runtime-status", get(api_runtime_status))
        .route("/api/sync-activity", get(api_sync_activity))
        .route("/api/sync-now", post(api_sync_now))
        .route("/api/sync-source-now", post(api_sync_source_now))
        .route("/api/sync-destination-now", post(api_sync_destination_now))
        .route("/api/cancel-activity", post(api_cancel_activity))
        .route(
            "/api/notify-status-changed",
            post(api_notify_status_changed),
        )
        .route("/api/scan-destination-now", post(api_scan_destination_now))
        .route("/api/scan-report", get(api_scan_report))
        .route("/api/tasks", get(api_tasks))
        .route("/api/all-tasks", get(api_all_tasks))
        .route("/api/task-wait", get(api_task_wait))
        .route(
            "/api/dismiss-restart-notice",
            post(api_dismiss_restart_notice),
        )
        .route("/api/transfer/snapshot", post(api_transfer_snapshot))
        .route(
            "/api/transfer/snapshot-paths",
            post(api_transfer_snapshot_paths),
        )
        .route("/api/transfer/path-info", post(api_transfer_path_info))
        .route("/api/transfer/prepare-dir", post(api_transfer_prepare_dir))
        .route(
            "/api/transfer/prepare-dirs",
            post(api_transfer_prepare_dirs),
        )
        .route(
            "/api/transfer/set-dir-mtimes",
            post(api_transfer_set_dir_mtimes),
        )
        .route("/api/transfer/set-modes", post(api_transfer_set_modes))
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
        .layer(middleware::from_fn(require_peer_token))
        // Bounded instead of disabled: the largest legitimate request bodies
        // are 16MiB transfer chunks and (worst case) a whole-file delta.
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(backend)
}

/// How often the LAN-address watcher re-detects the preferred local IP.
const LAN_BIND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Serve the web API on loopback plus the detected LAN address — deliberately
/// NOT 0.0.0.0, so the port is not exposed on other interfaces (VPN,
/// WAN-facing NICs).
///
/// Loopback binds immediately. The LAN address is detected and bound by a
/// watcher loop: at startup this usually binds it on the first pass, but it
/// also covers a network that comes up AFTER the daemon (boot before DHCP
/// finished) and a DHCP address change at runtime — the new address is bound
/// within one poll interval, no restart needed. A listener on a since-removed
/// address is harmless (it simply never receives connections), and peers learn
/// the new address through discovery.
pub async fn serve(backend: Backend, port: u16) -> Result<()> {
    let _discovery = spawn_discovery_responder(backend.config_path(), backend.port());
    let app = router(backend);

    let loopback = SocketAddr::from(([127, 0, 0, 1], port));
    let loopback_listener = tokio::net::TcpListener::bind(loopback)
        .await
        .map_err(|err| anyhow::anyhow!("failed to bind web listener on {loopback}: {err}"))?;
    info!(url = %format!("http://{loopback}/"), "auto_sync web listening");

    // LAN watcher: detect-then-bind, re-checked periodically.
    let lan_app = app.clone();
    tokio::spawn(async move {
        let mut bound: Vec<std::net::IpAddr> = vec![loopback.ip()];
        loop {
            let host = crate::core::config::preferred_local_host();
            if let Ok(ip) = host.parse::<std::net::IpAddr>() {
                if !ip.is_loopback() && !bound.contains(&ip) {
                    let addr = SocketAddr::new(ip, port);
                    match tokio::net::TcpListener::bind(addr).await {
                        Ok(listener) => {
                            let url = format!("http://{addr}/");
                            info!(url = %url, "auto_sync web listening");
                            println!("auto_sync Web UI: {url}");
                            bound.push(ip);
                            let app = lan_app.clone();
                            tokio::spawn(async move {
                                if let Err(err) = axum::serve(listener, app).await {
                                    tracing::warn!(%addr, error = %err, "LAN web listener stopped");
                                }
                            });
                        }
                        Err(err) => {
                            tracing::warn!(%addr, error = %err, "failed to bind LAN web listener; retrying");
                        }
                    }
                }
            }
            tokio::time::sleep(LAN_BIND_POLL_INTERVAL).await;
        }
    });

    axum::serve(loopback_listener, app)
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
    blocking(move || Ok(Json(backend.get_config()?))).await
}

async fn api_save_config(
    AxumState(backend): AxumState<Backend>,
    Json(cfg): Json<AppConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    blocking(move || Ok(Json(backend.save_config(&cfg)?))).await
}

async fn api_delegated_source_groups(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<DelegatedSourceGroupsRequest>,
) -> Result<Json<AppConfig>, ApiError> {
    blocking(move || Ok(Json(backend.apply_delegated_source_groups(req)?))).await
}

async fn api_health(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineHealth>, ApiError> {
    blocking(move || Ok(Json(backend.health()?))).await
}

async fn api_machines(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineStatus>, ApiError> {
    blocking(move || Ok(Json(backend.machines()?))).await
}

async fn api_discover_machines(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<MachineStatus>, ApiError> {
    blocking(move || Ok(Json(backend.discover_machines()?))).await
}

async fn api_add_machine(
    AxumState(backend): AxumState<Backend>,
    Json(machine): Json<MachineConfig>,
) -> Result<Json<AppConfig>, ApiError> {
    blocking(move || Ok(Json(backend.add_machine(machine)?))).await
}

async fn api_remove_machine(
    AxumState(backend): AxumState<Backend>,
    AxumPath(machine_id): AxumPath<String>,
) -> Result<Json<AppConfig>, ApiError> {
    blocking(move || Ok(Json(backend.remove_machine(&machine_id)?))).await
}

async fn api_status(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    blocking(move || Ok(Json(backend.status()?))).await
}

async fn api_runtime_status(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<RuntimeStatus>, ApiError> {
    // Reads in-memory atomics/progress only; safe to serve on the async path.
    Ok(Json(backend.runtime_status()))
}

async fn api_sync_activity(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<SyncActivityStatus>, ApiError> {
    blocking(move || Ok(Json(backend.sync_activity()?))).await
}

async fn api_sync_now(
    AxumState(backend): AxumState<Backend>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    blocking(move || Ok(Json(backend.sync_now()?))).await
}

#[derive(Debug, Deserialize)]
struct SyncSourceRequest {
    source_id: String,
}

async fn api_sync_source_now(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<SyncSourceRequest>,
) -> Result<Json<Vec<DestinationView>>, ApiError> {
    blocking(move || Ok(Json(backend.sync_source_now(&req.source_id)?))).await
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
    blocking(move || {
        Ok(Json(backend.sync_destination_now(
            &req.source_id,
            &req.destination_id,
            mode,
        )?))
    })
    .await
}

async fn api_notify_status_changed() -> Json<serde_json::Value> {
    // A peer's sync state changed: bump the epoch so the local UI re-fetches
    // statuses on its next (1s) runtime poll instead of waiting for the
    // slower status poll. Never re-pushed — see peer_notify.
    crate::core::peer_notify::note_remote_change();
    Json(serde_json::json!({ "ok": true }))
}

async fn api_cancel_activity(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<CancelActivityRequest>,
) -> Result<Json<CancelOutcome>, ApiError> {
    blocking(move || {
        let target = match (req.source_id.as_deref(), req.destination_id.as_deref()) {
            (Some(source_id), Some(destination_id)) => Some((source_id, destination_id)),
            _ => None,
        };
        Ok(Json(backend.cancel_activity(
            req.scope.as_deref(),
            target,
            req.propagate,
        )?))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ScanDestinationRequest {
    source_id: String,
    destination_id: String,
}

async fn api_scan_destination_now(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<ScanDestinationRequest>,
) -> Result<Json<Option<ScanReport>>, ApiError> {
    blocking(move || {
        Ok(Json(backend.scan_destination_now(
            &req.source_id,
            &req.destination_id,
        )?))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct ScanReportQuery {
    source_id: String,
    destination_id: String,
}

async fn api_scan_report(
    AxumState(backend): AxumState<Backend>,
    Query(query): Query<ScanReportQuery>,
) -> Result<Json<Option<ScanReport>>, ApiError> {
    blocking(move || {
        Ok(Json(
            backend.scan_report(&query.source_id, &query.destination_id)?,
        ))
    })
    .await
}

async fn api_dismiss_restart_notice(
    AxumState(backend): AxumState<Backend>,
    Json(req): Json<crate::core::backend::DismissRestartNoticeRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    blocking(move || {
        backend.dismiss_restart_notice(&req.source_id)?;
        Ok(Json(serde_json::json!({ "ok": true })))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct TasksQuery {
    limit: Option<usize>,
}

async fn api_tasks(
    AxumState(backend): AxumState<Backend>,
    Query(query): Query<TasksQuery>,
) -> Result<Json<Vec<crate::core::state::TaskLogEntry>>, ApiError> {
    blocking(move || {
        Ok(Json(
            backend.recent_tasks(query.limit.unwrap_or(100).min(100))?,
        ))
    })
    .await
}

async fn api_all_tasks(
    AxumState(backend): AxumState<Backend>,
    Query(query): Query<TasksQuery>,
) -> Result<Json<Vec<crate::core::backend::MachineTasksView>>, ApiError> {
    blocking(move || {
        Ok(Json(
            backend.all_tasks(query.limit.unwrap_or(100).min(100))?,
        ))
    })
    .await
}

#[derive(Debug, Deserialize)]
struct TaskWaitQuery {
    id: i64,
    timeout_secs: Option<u64>,
}

/// Long poll: blocks until the task leaves `running` or the timeout elapses,
/// returning the row's current state either way (null for an unknown id).
async fn api_task_wait(
    AxumState(backend): AxumState<Backend>,
    Query(query): Query<TaskWaitQuery>,
) -> Result<Json<Option<crate::core::state::TaskLogEntry>>, ApiError> {
    blocking(move || {
        Ok(Json(
            backend.wait_task(query.id, query.timeout_secs.unwrap_or(120))?,
        ))
    })
    .await
}

async fn api_transfer_snapshot(
    Json(req): Json<TransferSnapshotRequest>,
) -> Result<Json<Vec<SnapshotEntry>>, ApiError> {
    blocking(move || Ok(Json(transfer_snapshot(req)?))).await
}

async fn api_transfer_snapshot_paths(
    Json(req): Json<TransferSnapshotPathsRequest>,
) -> Result<Json<Vec<SnapshotEntry>>, ApiError> {
    blocking(move || Ok(Json(transfer_snapshot_paths(req)?))).await
}

async fn api_transfer_path_info(
    Json(req): Json<TransferPathInfoRequest>,
) -> Result<Json<TransferPathInfo>, ApiError> {
    blocking(move || Ok(Json(transfer_path_info(req)?))).await
}

async fn api_transfer_prepare_dir(
    Json(req): Json<TransferPrepareDirRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_prepare_dir(req)?))).await
}

async fn api_transfer_prepare_dirs(
    Json(req): Json<TransferPrepareDirsRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_prepare_dirs(req)?))).await
}

async fn api_transfer_set_dir_mtimes(
    Json(req): Json<TransferSetDirMtimesRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_set_dir_mtimes(req)?))).await
}

async fn api_transfer_set_modes(
    Json(req): Json<TransferSetModesRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_set_modes(req)?))).await
}

async fn api_transfer_remove_path(
    Json(req): Json<TransferRemovePathRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_remove_path(req)?))).await
}

async fn api_transfer_remove_paths(
    Json(req): Json<TransferRemovePathsRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_remove_paths(req)?))).await
}

async fn api_transfer_cleanup_tmp(
    Json(req): Json<TransferCleanupTmpRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_cleanup_tmp(req)?))).await
}

async fn api_transfer_put_file(
    Query(query): Query<TransferPutFileQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_put_file(query, &body)?))).await
}

async fn api_transfer_block_sums(
    Json(req): Json<TransferBlockSumsRequest>,
) -> Result<Json<crate::core::sync::delta::BlockSums>, ApiError> {
    blocking(move || Ok(Json(transfer_block_sums(req)?))).await
}

async fn api_transfer_apply_delta(
    Query(query): Query<TransferApplyDeltaQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_apply_delta(query, &body)?))).await
}

async fn api_transfer_file_offset(
    Json(req): Json<crate::core::sync::TransferFileOffsetRequest>,
) -> Result<Json<crate::core::sync::TransferFileOffset>, ApiError> {
    blocking(move || Ok(Json(transfer_file_offset(req)?))).await
}

async fn api_transfer_receive_file_chunk(
    Query(query): Query<TransferReceiveFileChunkQuery>,
    body: Bytes,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_receive_file_chunk(query, &body)?))).await
}

async fn api_transfer_finish_file(
    Json(req): Json<crate::core::sync::TransferFinishFileRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_finish_file(req)?))).await
}

async fn api_transfer_receive_symlink(
    Json(req): Json<TransferReceiveSymlinkRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_receive_symlink(req)?))).await
}

async fn api_transfer_push_file(
    Json(req): Json<TransferPushFileRequest>,
) -> Result<Json<TransferAck>, ApiError> {
    blocking(move || Ok(Json(transfer_push_file(req)?))).await
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
    blocking(move || Ok(Json(backend.browse_paths(query.path, query.machine_id)?))).await
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

/// Run a blocking handler body on Tokio's blocking thread pool.
///
/// Handlers call synchronous code (rusqlite, filesystem walks, blocking peer
/// HTTP) that can take seconds to minutes while a sync holds the DB. Running it
/// directly on an async worker thread starves the runtime: once every worker is
/// parked in blocking code, no request completes -- not even static routes -- so
/// the whole server appears hung (connections pile up in CLOSE-WAIT). Off-loading
/// to the blocking pool keeps the async workers free to accept and serve.
async fn blocking<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|err| ApiError(anyhow::anyhow!("request worker failed: {err}")))?
}
