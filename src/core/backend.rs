use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::core::cancel;
use crate::core::config::{
    AppConfig, MachineConfig, SourceGroupConfig, clean_config_for_save, config_warnings,
    load_config, load_or_create_config, machine_id_or_local, machine_is_local, machine_is_self,
    machine_matches_reference, save_config,
};
use crate::core::machines::{
    MachineHealth, MachineStatus, configure_tcp_connection_pool, discover_lan,
    encode_query_component, find_machine, local_health, machine_id_from_path,
    machine_matches_health, merge_discovered, remote_get_json, remote_post_json,
};
use crate::core::progress::{
    ScanProgressView, TransferProgressView, configure_progress_file, current_scan_progress,
    current_scan_progresses, current_transfer_progress,
};
use crate::core::state::{DestinationView, ScanReport, State as DbState};
use crate::core::sync::{SyncRequestMode, current_sync_kind, sync_is_running};

const DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MANUAL_DISCOVERY_MIN_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Backend {
    config_path: Arc<PathBuf>,
    port: u16,
    machine_cache: Arc<Mutex<MachineCache>>,
}

#[derive(Default)]
struct MachineCache {
    status: Option<MachineStatus>,
    refreshed_at: Option<Instant>,
}

impl Backend {
    pub fn new(config_path: PathBuf, port: u16) -> Self {
        let backend = Self {
            config_path: Arc::new(config_path),
            port,
            machine_cache: Arc::new(Mutex::new(MachineCache::default())),
        };
        backend.spawn_machine_discovery_worker();
        backend
    }

    pub fn config_path(&self) -> Arc<PathBuf> {
        self.config_path.clone()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn get_config(&self) -> Result<AppConfig> {
        let cfg = load_or_create_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        Ok(cfg)
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<AppConfig> {
        let current = load_config(&self.config_path)
            .ok()
            .map(|cfg| clean_config_for_save(&cfg));
        let mut cfg = preserve_current_machines(&self.config_path, cfg);
        normalize_local_machine_config(&mut cfg, &[]);
        reject_locked_source_path_changes(&self.config_path, &cfg)?;
        let next = clean_config_for_save(&cfg);
        let cfg = save_config(&self.config_path, &cfg)?;
        apply_runtime_config(&cfg);
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        if let Some(current) = current.as_ref() {
            reset_changed_destination_offsets(&state_db, current, &next)?;
        }
        self.propagate_remote_source_groups(current.as_ref(), &cfg)?;
        self.clear_machine_cache();
        Ok(cfg)
    }

    pub fn apply_delegated_source_groups(
        &self,
        req: DelegatedSourceGroupsRequest,
    ) -> Result<AppConfig> {
        let mut cfg = load_or_create_config(&self.config_path)?;
        cfg.source_groups
            .retain(|source| source.managed_by != req.controller_id);
        for mut source in req.source_groups {
            source.managed_by = req.controller_id.clone();
            cfg.source_groups.push(source);
        }
        merge_delegated_machines(&mut cfg, &req.machines);
        normalize_local_machine_config(&mut cfg, &req.machines);
        let cfg = save_config(&self.config_path, &cfg)?;
        apply_runtime_config(&cfg);
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        self.clear_machine_cache();
        Ok(cfg)
    }

    pub fn health(&self) -> Result<MachineHealth> {
        let cfg = load_or_create_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        Ok(local_health(&cfg, self.port))
    }

    pub fn machines(&self) -> Result<MachineStatus> {
        if let Some(status) = self.cached_machine_status() {
            return Ok(status);
        }
        self.refresh_machine_cache(DISCOVERY_REFRESH_INTERVAL)
    }

    pub fn discover_machines(&self) -> Result<MachineStatus> {
        self.refresh_machine_cache(MANUAL_DISCOVERY_MIN_INTERVAL)
    }

    pub fn add_machine(&self, machine: MachineConfig) -> Result<AppConfig> {
        let mut cfg = load_or_create_config(&self.config_path)?;
        if let Some(existing) = cfg.machines.iter_mut().find(|item| {
            non_empty_machine_match(item, &machine.alias_name)
                || non_empty_machine_match(item, &machine.id)
                || non_empty_machine_match(item, &machine.host)
        }) {
            *existing = machine;
        } else {
            cfg.machines.push(machine);
        }
        let cfg = save_config(&self.config_path, &cfg)?;
        apply_runtime_config(&cfg);
        self.clear_machine_cache();
        Ok(cfg)
    }

    pub fn remove_machine(&self, machine_id: &str) -> Result<AppConfig> {
        if machine_id == "local" {
            anyhow::bail!("local machine cannot be deleted");
        }
        let mut cfg = load_or_create_config(&self.config_path)?;
        cfg.machines.retain(|machine| machine.id != machine_id);
        let cfg = save_config(&self.config_path, &cfg)?;
        apply_runtime_config(&cfg);
        self.clear_machine_cache();
        Ok(cfg)
    }

    pub fn status(&self) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        let local = state_db.destination_views(&cfg)?;
        self.merge_remote_source_statuses(&cfg, local)
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        RuntimeStatus {
            syncing: sync_is_running(),
            sync_kind: current_sync_kind(),
            transfer: current_transfer_progress(),
            scan: current_scan_progress(),
            scans: current_scan_progresses(),
            build: BuildInfo::current(),
            config_errors: current_config_warnings(),
            status_epoch: crate::core::peer_notify::status_epoch(),
        }
    }

    pub fn sync_activity(&self) -> Result<SyncActivityStatus> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let mut machines = vec![MachineRuntimeView {
            machine_id: "local".to_string(),
            label: "local".to_string(),
            local: true,
            runtime: Some(self.runtime_status()),
            error: None,
        }];
        for (machine_id, machine) in remote_runtime_machine_refs(&cfg) {
            match remote_get_json::<RuntimeStatus>(
                &machine,
                "/api/runtime-status",
                Duration::from_secs(3),
            ) {
                Ok(runtime) => machines.push(MachineRuntimeView {
                    machine_id,
                    label: machine_label(&machine),
                    local: false,
                    runtime: Some(runtime),
                    error: None,
                }),
                Err(err) => machines.push(MachineRuntimeView {
                    machine_id,
                    label: machine_label(&machine),
                    local: false,
                    runtime: None,
                    error: Some(err.to_string()),
                }),
            }
        }
        Ok(SyncActivityStatus { machines })
    }

    pub fn sync_now(&self) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        for machine in remote_source_machines(&cfg) {
            let _: Vec<DestinationView> = remote_post_json(
                &machine,
                "/api/sync-now",
                &EmptyRequest {},
                Duration::from_secs(30),
            )?;
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        // Record the request synchronously so the returned statuses show the
        // new targets, then run the engine in the background: a busy engine
        // queues the work instead of rejecting it, and a long sync cannot
        // time out the caller (delegated requests included).
        state_db.force_target_all_destinations(&cfg)?;
        spawn_background_sync(cfg.clone(), "incremental");
        self.merge_remote_source_statuses(&cfg, state_db.destination_views(&cfg)?)
    }

    pub fn sync_source_now(&self, source_id: &str) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        if let Some(machine) = source_execution_machine(&cfg, source_id)? {
            let _: Vec<DestinationView> = remote_post_json(
                &machine,
                "/api/sync-source-now",
                &SyncSourceRequest {
                    source_id: source_id.to_string(),
                },
                Duration::from_secs(30),
            )?;
            return self.status();
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        state_db.force_target_source(&cfg, source_id)?;
        spawn_background_sync(cfg.clone(), "incremental");
        self.merge_remote_source_statuses(&cfg, state_db.destination_views(&cfg)?)
    }

    pub fn sync_destination_now(
        &self,
        source_id: &str,
        destination_id: &str,
        mode: SyncRequestMode,
    ) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        if let Some(machine) = source_execution_machine(&cfg, source_id)? {
            let _: Vec<DestinationView> = remote_post_json(
                &machine,
                "/api/sync-destination-now",
                &SyncDestinationRequest {
                    source_id: source_id.to_string(),
                    destination_id: destination_id.to_string(),
                    mode: Some(sync_request_mode_wire_value(mode).to_string()),
                },
                Duration::from_secs(30),
            )?;
            return self.status();
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        crate::core::sync::queue_destination_sync(
            &cfg,
            &state_db,
            source_id,
            destination_id,
            mode,
        )?;
        spawn_background_sync(cfg.clone(), sync_request_mode_wire_value(mode));
        self.merge_remote_source_statuses(&cfg, state_db.destination_views(&cfg)?)
    }

    /// Cancel long-running activity (sync passes, compares, and the tree
    /// walks they run — including walks served for a peer). `scope` limits
    /// the cancel to "sync" or "compare"; `None` cancels both. `target`
    /// (source_id + destination_id) limits it to work scoped to that
    /// destination. When `propagate` is set the request is forwarded to every
    /// known runtime machine (best effort), so the machine actually burning
    /// disk time stops too; forwarded requests arrive with `propagate =
    /// false` and stay local.
    pub fn cancel_activity(
        &self,
        scope: Option<&str>,
        target: Option<(&str, &str)>,
        propagate: bool,
    ) -> Result<CancelOutcome> {
        let target_label =
            target.map(|(source_id, destination_id)| cancel::target_for(source_id, destination_id));
        let cancelled_local = cancel::request(scope, target_label.as_deref());
        let mut machines = Vec::new();
        if propagate {
            let cfg = load_config(&self.config_path)?;
            apply_runtime_config(&cfg);
            let req = CancelActivityRequest {
                scope: scope.map(ToString::to_string),
                source_id: target.map(|(source_id, _)| source_id.to_string()),
                destination_id: target.map(|(_, destination_id)| destination_id.to_string()),
                propagate: false,
            };
            for (machine_id, machine) in remote_runtime_machine_refs(&cfg) {
                match remote_post_json::<_, CancelOutcome>(
                    &machine,
                    "/api/cancel-activity",
                    &req,
                    Duration::from_secs(5),
                ) {
                    Ok(outcome) => machines.push(MachineCancelView {
                        machine_id,
                        cancelled: outcome.cancelled_local,
                        error: None,
                    }),
                    Err(err) => {
                        warn!(machine = machine_id, error = %err, "failed to propagate cancel");
                        machines.push(MachineCancelView {
                            machine_id,
                            cancelled: 0,
                            error: Some(err.to_string()),
                        });
                    }
                }
            }
        }
        Ok(CancelOutcome {
            cancelled_local,
            machines,
        })
    }

    /// Start a dry-run Scan (compare source vs destination, no changes). The
    /// scan runs in the background — a large tree can take many minutes and must
    /// not hold the request open or block the backup — and persists its report;
    /// callers poll [`Self::scan_report`] for the result. Returns the previously
    /// stored report (if any) so the UI has something to show immediately.
    /// Delegated to the source's machine for a remote source.
    pub fn scan_destination_now(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<ScanReport>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        if let Some(machine) = source_execution_machine(&cfg, source_id)? {
            return remote_post_json(
                &machine,
                "/api/scan-destination-now",
                &ScanDestinationRequest {
                    source_id: source_id.to_string(),
                    destination_id: destination_id.to_string(),
                },
                Duration::from_secs(30),
            );
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        let previous = state_db.get_scan_report(source_id, destination_id)?;
        let cfg = cfg.clone();
        let source_id = source_id.to_string();
        let destination_id = destination_id.to_string();
        std::thread::spawn(move || match DbState::open(&cfg.app.data_db) {
            Ok(state) => {
                let action_started_at = chrono::Utc::now().to_rfc3339();
                let task_id = state
                    .task_start("compare", &source_id, &destination_id)
                    .ok();
                let result = crate::core::sync::scan_destination_now(
                    &cfg,
                    &state,
                    &source_id,
                    &destination_id,
                );
                // A compare that STARTED after the restart notice was raised
                // read the post-gap tree: whatever the gap changed shows up
                // in its report, so the notice has served its purpose.
                if result.is_ok() {
                    if let Err(err) =
                        state.clear_restart_notice_if_covered(&source_id, &action_started_at)
                    {
                        tracing::warn!(error = %err, "failed to clear restart notice");
                    }
                }
                if let Some(task_id) = task_id {
                    let record = match &result {
                        Ok(report) => {
                            let differences = report.to_add
                                + report.to_update
                                + report.to_delete
                                + report.type_mismatch;
                            let entries = report.source_entries + report.dst_entries;
                            state.task_finish(task_id, "success", "", 0, differences, entries)
                        }
                        // The scan never ran (another one holds the gate):
                        // nothing worth a history row.
                        Err(err) if crate::core::sync::scan_error_is_already_running(err) => {
                            state.task_discard(task_id)
                        }
                        Err(err) if cancel::error_is_cancelled(err) => state.task_finish(
                            task_id,
                            "cancelled",
                            cancel::CANCELLED_MESSAGE,
                            0,
                            0,
                            0,
                        ),
                        Err(err) => {
                            state.task_finish(task_id, "failed", &format!("{err:#}"), 0, 0, 0)
                        }
                    };
                    if let Err(err) = record {
                        tracing::warn!(error = %err, "failed to record compare task log entry");
                    }
                }
                if let Err(err) = result {
                    tracing::warn!(error = %err, source = source_id, destination = destination_id, "scan failed");
                    // Persist the failure (with a fresh scanned_at) so the UI's
                    // report poll terminates and surfaces the error instead of
                    // showing "compare running" forever. A concurrent-scan
                    // rejection must not clobber the other scan's report.
                    if !crate::core::sync::scan_error_is_already_running(&err) {
                        let failure = ScanReport {
                            source_id: source_id.clone(),
                            destination_id: destination_id.clone(),
                            scanned_at: chrono::Utc::now().to_rfc3339(),
                            error: format!("{err:#}").chars().take(500).collect(),
                            ..Default::default()
                        };
                        if let Err(err) = state.put_scan_report(&failure) {
                            tracing::warn!(error = %err, "failed to persist scan failure report");
                        }
                    }
                }
            }
            Err(err) => tracing::warn!(error = %err, "scan could not open state db"),
        });
        Ok(previous)
    }

    /// Dismiss the restart notice for a source (the user chose to ignore the
    /// possible gap). Delegated to the source's machine — the notice lives in
    /// its state db.
    pub fn dismiss_restart_notice(&self, source_id: &str) -> Result<()> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        if let Some(machine) = source_execution_machine(&cfg, source_id)? {
            let _: serde_json::Value = remote_post_json(
                &machine,
                "/api/dismiss-restart-notice",
                &DismissRestartNoticeRequest {
                    source_id: source_id.to_string(),
                },
                Duration::from_secs(10),
            )?;
            return Ok(());
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        state_db.dismiss_restart_notice(source_id)
    }

    /// Recent task-log rows (running first by recency). Tasks are recorded on
    /// the machine that executes them — query each machine for its own log.
    pub fn recent_tasks(&self, limit: usize) -> Result<Vec<crate::core::state::TaskLogEntry>> {
        let cfg = load_config(&self.config_path)?;
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        state_db.recent_tasks(limit)
    }

    /// The most recent stored Scan report for a destination, if any.
    pub fn scan_report(&self, source_id: &str, destination_id: &str) -> Result<Option<ScanReport>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        if let Some(machine) = source_execution_machine(&cfg, source_id)? {
            let path = format!(
                "/api/scan-report?source_id={}&destination_id={}",
                encode_query_component(source_id),
                encode_query_component(destination_id),
            );
            return remote_get_json::<Option<ScanReport>>(&machine, &path, Duration::from_secs(10));
        }
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.get_scan_report(source_id, destination_id)
    }

    pub fn browse_paths(
        &self,
        path: Option<PathBuf>,
        machine_id: Option<String>,
    ) -> Result<BrowseResponse> {
        let machine_id = machine_id_from_path(machine_id.as_deref());
        if machine_id != "local" {
            let cfg = load_config(&self.config_path)?;
            apply_runtime_config(&cfg);
            let machine = find_machine(&cfg, machine_id)
                .ok_or_else(|| anyhow::anyhow!("unknown machine: {machine_id}"))?;
            let requested = path.unwrap_or_else(|| default_path_for_os(&machine.os));
            let path = format!(
                "/api/browse-paths?path={}",
                encode_query_component(&requested.to_string_lossy())
            );
            return remote_get_json::<BrowseResponse>(&machine, &path, Duration::from_secs(3));
        }
        browse_paths_inner(path.unwrap_or_else(|| PathBuf::from("/")))
    }

    fn spawn_machine_discovery_worker(&self) {
        let backend = self.clone();
        let result = thread::Builder::new()
            .name("auto_sync_machine_discovery".to_string())
            .spawn(move || {
                loop {
                    if let Err(err) = backend.refresh_machine_cache(DISCOVERY_REFRESH_INTERVAL) {
                        warn!(error = %err, "machine discovery refresh failed");
                    }
                    thread::sleep(DISCOVERY_REFRESH_INTERVAL);
                }
            });
        if let Err(err) = result {
            warn!(error = %err, "failed to spawn machine discovery worker");
        }
    }

    fn refresh_machine_cache(&self, min_interval: Duration) -> Result<MachineStatus> {
        {
            let cache = self
                .machine_cache
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            if let (Some(status), Some(refreshed_at)) = (&cache.status, cache.refreshed_at) {
                if refreshed_at.elapsed() < min_interval {
                    return Ok(status.clone());
                }
            }
        }

        // Discover without holding the cache lock (the sweep takes ~700ms, and
        // persisting a hostname fix below clears the cache -- same lock).
        let mut cfg = load_or_create_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let discovered = discover_lan(Duration::from_millis(700))?;
        if refresh_machine_metadata_from_health(&mut cfg, &discovered) {
            // A machine reported different metadata (rename, moved endpoint, new
            // ssh port, ...); persist it. Use the plain config save, not the
            // delegating one -- syncing discovered metadata needn't re-push.
            cfg = save_config(&self.config_path, &cfg)?;
        }
        let status = merge_discovered(&cfg, discovered);
        {
            let mut cache = self
                .machine_cache
                .lock()
                .unwrap_or_else(|err| err.into_inner());
            cache.status = Some(status.clone());
            cache.refreshed_at = Some(Instant::now());
        }
        Ok(status)
    }

    fn cached_machine_status(&self) -> Option<MachineStatus> {
        self.machine_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .status
            .clone()
    }

    fn clear_machine_cache(&self) {
        let mut cache = self
            .machine_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        cache.status = None;
        cache.refreshed_at = None;
    }

    fn propagate_remote_source_groups(
        &self,
        previous: Option<&AppConfig>,
        cfg: &AppConfig,
    ) -> Result<()> {
        let controller_id = local_health(cfg, self.port).id;
        let mut source_machines = Vec::<String>::new();
        for source in previous
            .into_iter()
            .flat_map(|cfg| cfg.source_groups.iter())
            .chain(cfg.source_groups.iter())
        {
            let machine_id = machine_id_or_local(&source.machine_id);
            if machine_id != "local" && !source_machines.iter().any(|id| id == machine_id) {
                source_machines.push(machine_id.to_string());
            }
        }

        for source_machine_id in source_machines {
            let machine = find_machine(cfg, &source_machine_id)
                .or_else(|| {
                    previous.and_then(|previous| find_machine(previous, &source_machine_id))
                })
                .ok_or_else(|| anyhow::anyhow!("unknown source machine: {source_machine_id}"))?;
            let groups = delegated_groups_for_machine(cfg, &source_machine_id, &controller_id)?;
            let req = DelegatedSourceGroupsRequest {
                controller_id: controller_id.clone(),
                machines: cfg.machines.clone(),
                source_groups: groups,
            };
            let _: AppConfig = remote_post_json(
                &machine,
                "/api/config/delegated-source-groups",
                &req,
                Duration::from_secs(5),
            )
            .with_context(|| format!("failed to push source groups to {source_machine_id}"))?;
        }
        Ok(())
    }

    fn merge_remote_source_statuses(
        &self,
        cfg: &AppConfig,
        local: Vec<DestinationView>,
    ) -> Result<Vec<DestinationView>> {
        let remote_source_ids: Vec<String> = cfg
            .source_groups
            .iter()
            .filter(|source| machine_id_or_local(&source.machine_id) != "local")
            .map(|source| source.id.clone())
            .collect();
        if remote_source_ids.is_empty() {
            return Ok(local);
        }

        let mut views: Vec<DestinationView> = local
            .into_iter()
            .filter(|view| !remote_source_ids.iter().any(|id| id == &view.source_id))
            .collect();
        for source in cfg
            .source_groups
            .iter()
            .filter(|source| machine_id_or_local(&source.machine_id) != "local")
        {
            let machine_id = machine_id_or_local(&source.machine_id);
            let Some(machine) = find_machine(cfg, machine_id) else {
                views.extend(offline_views_for_source(source, "unknown_source_machine"));
                continue;
            };
            match remote_get_json::<Vec<DestinationView>>(
                &machine,
                "/api/status",
                Duration::from_secs(3),
            ) {
                Ok(remote_views) => {
                    let wanted: Vec<DestinationView> = remote_views
                        .into_iter()
                        .filter(|view| view.source_id == source.id)
                        .collect();
                    if wanted.is_empty() {
                        views.extend(offline_views_for_source(source, "remote_status_missing"));
                    } else {
                        views.extend(wanted);
                    }
                }
                Err(err) => {
                    warn!(source = source.id, machine = machine_id, error = %err, "failed to fetch remote source status");
                    views.extend(offline_views_for_source(source, "source_machine_offline"));
                }
            }
        }
        Ok(views)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegatedSourceGroupsRequest {
    pub controller_id: String,
    pub machines: Vec<MachineConfig>,
    pub source_groups: Vec<crate::core::config::SourceGroupConfig>,
}

#[derive(Debug, Serialize)]
struct SyncSourceRequest {
    source_id: String,
}

#[derive(Debug, Serialize)]
struct SyncDestinationRequest {
    source_id: String,
    destination_id: String,
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScanDestinationRequest {
    source_id: String,
    destination_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DismissRestartNoticeRequest {
    pub source_id: String,
}

#[derive(Debug, Serialize)]
struct EmptyRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelActivityRequest {
    /// "sync", "compare", or absent for both.
    #[serde(default)]
    pub scope: Option<String>,
    /// Together with `destination_id`, limits the cancel to work scoped to
    /// one destination; absent cancels regardless of destination.
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub destination_id: Option<String>,
    /// Forward the cancel to peer machines. Defaults to true for UI/API
    /// callers; propagated requests carry false to stop the fan-out.
    #[serde(default = "default_true")]
    pub propagate: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelOutcome {
    /// How many local operations were signalled to stop.
    pub cancelled_local: usize,
    /// Per-machine propagation results (empty when not propagating).
    #[serde(default)]
    pub machines: Vec<MachineCancelView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineCancelView {
    pub machine_id: String,
    pub cancelled: usize,
    pub error: Option<String>,
}

fn delegated_groups_for_machine(
    cfg: &AppConfig,
    source_machine_id: &str,
    controller_id: &str,
) -> Result<Vec<SourceGroupConfig>> {
    let source_machine = find_machine(cfg, source_machine_id)
        .ok_or_else(|| anyhow::anyhow!("unknown source machine: {source_machine_id}"))?;
    let mut groups = Vec::new();
    for source in cfg
        .source_groups
        .iter()
        .filter(|source| machine_id_or_local(&source.machine_id) == source_machine_id)
    {
        let mut delegated = source.clone();
        delegated.machine_id = "local".to_string();
        delegated.managed_by = controller_id.to_string();
        for dst in &mut delegated.destinations {
            let dst_machine_id = machine_id_or_local(&dst.machine_id);
            if dst_machine_id == source_machine_id
                || machine_matches_reference(&source_machine, &dst.machine_id)
            {
                dst.machine_id = "local".to_string();
            }
        }
        groups.push(delegated);
    }
    Ok(groups)
}

fn source_execution_machine(cfg: &AppConfig, source_id: &str) -> Result<Option<MachineConfig>> {
    let Some(source) = cfg
        .source_groups
        .iter()
        .find(|source| source.id == source_id)
    else {
        return Ok(None);
    };
    let machine_id = machine_id_or_local(&source.machine_id);
    if machine_id == "local" {
        return Ok(None);
    }
    Ok(Some(find_machine(cfg, machine_id).ok_or_else(|| {
        anyhow::anyhow!("unknown source machine: {machine_id}")
    })?))
}

fn remote_source_machines(cfg: &AppConfig) -> Vec<MachineConfig> {
    remote_source_machine_refs(cfg)
        .into_iter()
        .map(|(_, machine)| machine)
        .collect()
}

fn remote_source_machine_refs(cfg: &AppConfig) -> Vec<(String, MachineConfig)> {
    let mut machines = Vec::new();
    for source in cfg
        .source_groups
        .iter()
        .filter(|source| machine_id_or_local(&source.machine_id) != "local")
    {
        let machine_id = machine_id_or_local(&source.machine_id);
        if machines
            .iter()
            .any(|(_, machine): &(String, MachineConfig)| {
                machine_matches_reference(machine, machine_id)
            })
        {
            continue;
        }
        if let Some(machine) = find_machine(cfg, machine_id) {
            machines.push((machine_id.to_string(), machine));
        }
    }
    machines
}

fn remote_runtime_machine_refs(cfg: &AppConfig) -> Vec<(String, MachineConfig)> {
    let mut machines = remote_source_machine_refs(cfg);
    for source in &cfg.source_groups {
        for dst in source.destinations.iter().filter(|dst| dst.enabled) {
            let machine_id = machine_id_or_local(&dst.machine_id);
            if machine_id == "local" {
                continue;
            }
            if machines
                .iter()
                .any(|(_, machine): &(String, MachineConfig)| {
                    machine_matches_reference(machine, machine_id)
                })
            {
                continue;
            }
            if let Some(machine) = find_machine(cfg, machine_id) {
                machines.push((machine_id.to_string(), machine));
            }
        }
    }
    machines
}

fn offline_views_for_source(source: &SourceGroupConfig, reason: &str) -> Vec<DestinationView> {
    source
        .destinations
        .iter()
        .filter(|dst| dst.enabled)
        .map(|dst| DestinationView {
            source_id: source.id.clone(),
            destination_id: dst.id.clone(),
            path: dst.path.to_string_lossy().to_string(),
            enabled: dst.enabled,
            latest_closed_cycle_id: None,
            target_cycle_id: None,
            last_verified_cycle_id: None,
            last_completed_cycle_id: None,
            status: "red".to_string(),
            status_reason: reason.to_string(),
            updated_at: None,
            issues: Vec::new(),
            scan_differences: None,
            scan_at: None,
            restart_notice_at: None,
            restart_gap_started: None,
        })
        .collect()
}

fn merge_delegated_machines(cfg: &mut AppConfig, incoming: &[MachineConfig]) {
    for machine in incoming.iter().filter(|machine| machine.id != "local") {
        // A controller pushes its full machine list, which includes an entry for
        // THIS machine (e.g. the NAS receiving "nas"/its own LAN IP). Never add
        // that as a separate peer -- we already know ourselves as "local", and a
        // duplicate self-entry would make us try to sync/HTTP to ourselves.
        if machine_is_self(cfg, machine) {
            continue;
        }
        if let Some(existing) = cfg.machines.iter_mut().find(|existing| {
            non_empty_machine_match(existing, &machine.id)
                || non_empty_machine_match(existing, &machine.alias_name)
                || non_empty_machine_match(existing, &machine.host)
        }) {
            if existing.id != "local" {
                *existing = machine.clone();
            }
        } else {
            cfg.machines.push(machine.clone());
        }
    }
}

/// Adapt a config to the machine it now lives on: rewrite any source/destination
/// that points at this host (by "local", hostname, LAN IP, or a machine id/alias
/// that resolves to us) to the canonical "local" id, then drop duplicate machine
/// entries that are really this host. Idempotent and safe on every machine --
/// only entries that genuinely resolve to the *local* host are collapsed, so a
/// controller keeps its remote peers untouched.
fn normalize_local_machine_config(cfg: &mut AppConfig, reference_machines: &[MachineConfig]) {
    // Resolve references against our own machines plus any the controller sent,
    // since a self-machine the controller pushed is intentionally NOT kept in
    // our list (merge_delegated_machines drops it) yet a source/destination may
    // still reference it by that id.
    let mut snapshot = cfg.clone();
    snapshot.machines.extend(reference_machines.iter().cloned());
    let is_self_ref = |machine_id: &str| -> bool {
        if machine_is_local(&snapshot, machine_id) {
            return true;
        }
        snapshot
            .machines
            .iter()
            .any(|m| machine_matches_reference(m, machine_id) && machine_is_self(&snapshot, m))
    };
    for source in &mut cfg.source_groups {
        if is_self_ref(&source.machine_id) {
            source.machine_id = "local".to_string();
        }
        for dst in &mut source.destinations {
            if is_self_ref(&dst.machine_id) {
                dst.machine_id = "local".to_string();
            }
        }
    }
    // Drop any machine entry already in the config that is really this host.
    let prune_snapshot = cfg.clone();
    cfg.machines
        .retain(|m| m.id == "local" || !machine_is_self(&prune_snapshot, m));
}

fn sync_request_mode_wire_value(mode: SyncRequestMode) -> &'static str {
    match mode {
        SyncRequestMode::Incremental => "incremental",
        SyncRequestMode::Full => "full",
        SyncRequestMode::ChangedSince => "changed_since",
        SyncRequestMode::RepairScan => "repair_scan",
    }
}

/// Run the sync engine on a detached thread: waits for any running pass to
/// finish (so a queued manual request executes right afterwards) and drives
/// all pending cycles. Errors are logged; the destination statuses carry the
/// outcome for the UI's polling.
fn spawn_background_sync(cfg: AppConfig, kind: &str) {
    let kind = kind.to_string();
    std::thread::spawn(move || match DbState::open(&cfg.app.data_db) {
        Ok(mut state) => {
            if let Err(err) = crate::core::sync::run_pending_with_kind(&cfg, &mut state, &kind) {
                tracing::warn!(error = %err, kind, "manual sync run failed");
            }
        }
        Err(err) => tracing::warn!(error = %err, "manual sync could not open state db"),
    });
}

fn machine_label(machine: &MachineConfig) -> String {
    if !machine.alias_name.trim().is_empty() {
        return machine.alias_name.trim().to_string();
    }
    if !machine.name.trim().is_empty() {
        return machine.name.trim().to_string();
    }
    if !machine.host.trim().is_empty() {
        return machine.host.trim().to_string();
    }
    machine.id.clone()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStatus {
    #[serde(default)]
    pub syncing: bool,
    #[serde(default)]
    pub sync_kind: Option<String>,
    pub transfer: Option<TransferProgressView>,
    /// The most recently updated walk (single-slot status-bar display).
    pub scan: Option<ScanProgressView>,
    /// Every live walk, each attributed to its destination — concurrent walks
    /// (a compare while a peer-served or verify walk runs) all stay visible.
    #[serde(default)]
    pub scans: Vec<ScanProgressView>,
    pub build: BuildInfo,
    #[serde(default)]
    pub config_errors: Vec<String>,
    /// Bumps whenever this machine's sync state changes or a peer pushes a
    /// status notification; the UI re-fetches statuses when it moves.
    #[serde(default)]
    pub status_epoch: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncActivityStatus {
    pub machines: Vec<MachineRuntimeView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MachineRuntimeView {
    pub machine_id: String,
    pub label: String,
    pub local: bool,
    pub runtime: Option<RuntimeStatus>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildInfo {
    pub commit: String,
    pub commit_time_beijing: String,
}

impl BuildInfo {
    fn current() -> Self {
        Self {
            commit: option_env!("AUTO_SYNC_GIT_COMMIT_SHORT")
                .unwrap_or("unknown")
                .to_string(),
            commit_time_beijing: option_env!("AUTO_SYNC_GIT_COMMIT_TIME_BEIJING")
                .unwrap_or("unknown")
                .to_string(),
        }
    }
}

fn non_empty_machine_match(machine: &MachineConfig, value: &str) -> bool {
    !value.trim().is_empty() && machine_matches_reference(machine, value)
}

fn preserve_current_machines(config_path: &Path, cfg: &AppConfig) -> AppConfig {
    let mut cfg = cfg.clone();
    if let Ok(current) = load_config(config_path) {
        cfg.machines = current.machines;
    }
    cfg
}

fn reject_locked_source_path_changes(config_path: &Path, next: &AppConfig) -> Result<()> {
    let Ok(current) = load_config(config_path) else {
        return Ok(());
    };
    let current = clean_config_for_save(&current);
    let next = clean_config_for_save(next);
    for current_source in &current.source_groups {
        if current_source.destinations.is_empty() {
            continue;
        }
        let Some(next_source) = next
            .source_groups
            .iter()
            .find(|source| source.id == current_source.id)
        else {
            continue;
        };
        if current_source.src != next_source.src
            || current_source.machine_id != next_source.machine_id
        {
            anyhow::bail!(
                "source path is locked after adding a destination: {}",
                current_source.id
            );
        }
    }
    Ok(())
}

fn reset_changed_destination_offsets(
    state: &DbState,
    current: &AppConfig,
    next: &AppConfig,
) -> Result<()> {
    for current_source in &current.source_groups {
        let Some(next_source) = next
            .source_groups
            .iter()
            .find(|source| source.id == current_source.id)
        else {
            continue;
        };
        for current_dst in &current_source.destinations {
            let Some(next_dst) = next_source
                .destinations
                .iter()
                .find(|dst| dst.id == current_dst.id)
            else {
                continue;
            };
            if current_dst.path != next_dst.path || current_dst.machine_id != next_dst.machine_id {
                state.reset_destination_offset(
                    &current_source.id,
                    &current_dst.id,
                    "destination_path_changed",
                )?;
            }
        }
    }
    Ok(())
}

/// Update a stored remote machine's metadata from what it reports over
/// discovery, so a rename or moved endpoint stops showing stale values. Every
/// discovered field is authoritative EXCEPT `alias_name`, which is always
/// user-configured and never touched. SSH user/port are preferred from the
/// advertisement (each machine detects its own sshd port and the account it runs
/// as); only an empty user / zero port -- an advertisement carrying no info -- is
/// ignored. Returns true if anything changed.
fn refresh_machine_metadata_from_health(cfg: &mut AppConfig, discovered: &[MachineHealth]) -> bool {
    let mut changed = false;
    for machine in &mut cfg.machines {
        if machine.id == "local" {
            continue;
        }
        let Some(health) = discovered
            .iter()
            .find(|health| machine_matches_health(machine, health))
        else {
            continue;
        };

        let hostname = health.name.trim();
        if !hostname.is_empty()
            && hostname != "This machine"
            && !hostname.eq_ignore_ascii_case("local")
            && machine.name.trim() != hostname
        {
            machine.name = hostname.to_string();
            changed = true;
        }

        let host = health.host.trim();
        if !host.is_empty() && machine.host.trim() != host {
            machine.host = host.to_string();
            changed = true;
        }

        if health.port != 0 && machine.port != health.port {
            machine.port = health.port;
            changed = true;
        }

        let os = health.os.trim();
        if !os.is_empty() && !machine.os.trim().eq_ignore_ascii_case(os) {
            machine.os = os.to_string();
            changed = true;
        }

        let install_dir = health.install_dir.trim();
        if !install_dir.is_empty() && machine.install_dir.to_string_lossy() != install_dir {
            machine.install_dir = PathBuf::from(install_dir);
            changed = true;
        }

        // SSH: prefer what the machine advertises. It detects its own sshd port
        // and the account this process runs as, so the advertisement is the
        // source of truth for how to reach it. Only skip an empty user / zero
        // port (an advertisement that carries no information).
        let ssh_user = health.ssh_user.trim();
        if !ssh_user.is_empty() && machine.ssh_user.trim() != ssh_user {
            machine.ssh_user = ssh_user.to_string();
            changed = true;
        }

        if health.ssh_port != 0 && machine.ssh_port != health.ssh_port {
            machine.ssh_port = health.ssh_port;
            changed = true;
        }
    }
    changed
}

/// Latest non-fatal config problems, refreshed whenever the config is loaded or
/// saved and surfaced through runtime status for the UI status bar.
static CONFIG_WARNINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn apply_runtime_config(cfg: &AppConfig) {
    configure_tcp_connection_pool(cfg.app.tcp_connection_pool_size);
    configure_progress_file(&cfg.app.data_db);
    *CONFIG_WARNINGS
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = config_warnings(cfg);
}

fn current_config_warnings() -> Vec<String> {
    CONFIG_WARNINGS
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BrowseEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BrowseResponse {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<BrowseEntry>,
}

pub fn default_path_for_os(os: &str) -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_disk_machines_when_saving_stale_config() {
        let temp = temp_dir("backend_preserve_machines");
        let config_path = temp.join("auto_sync.toml");

        let mut stale = AppConfig::default();
        stale.app.data_db = temp.join("state").join("auto_sync.sqlite");
        stale.app.log_dir = temp.join("logs");
        stale.machines.push(MachineConfig {
            id: "windows".to_string(),
            alias_name: String::new(),
            name: "windows".to_string(),
            host: "192.168.3.7".to_string(),
            port: 18765,
            ssh_user: "Administrator".to_string(),
            ssh_port: 22,
            os: "windows".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        let mut current = stale.clone();
        current.machines.retain(|machine| machine.id != "windows");
        crate::core::config::save_config(&config_path, &current).unwrap();

        let merged = preserve_current_machines(&config_path, &stale);
        assert!(
            !merged
                .machines
                .iter()
                .any(|machine| machine.id == "windows")
        );
    }

    #[test]
    fn delegated_groups_execute_on_source_machine_as_local() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "nas".to_string(),
            alias_name: "nas".to_string(),
            name: "nas".to_string(),
            host: "192.0.2.20".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });
        cfg.source_groups
            .push(crate::core::config::SourceGroupConfig {
                id: "src_nas".to_string(),
                machine_id: "nas".to_string(),
                src: PathBuf::from("/zfs"),
                add_directory: false,
                managed_by: String::new(),
                excludes: Vec::new(),
                enabled: true,
                mode: crate::core::config::SyncMode::Mirror,
                snapshot: crate::core::config::SnapshotConfig::default(),
                destinations: vec![crate::core::config::DestinationConfig {
                    id: "dst_nas".to_string(),
                    machine_id: "nas".to_string(),
                    path: PathBuf::from("/zfs_pool"),
                    enabled: true,
                    schedule: crate::core::config::ScheduleConfig::default(),
                    sync: None,
                }],
            });

        let groups = delegated_groups_for_machine(&cfg, "nas", "controller-1").unwrap();

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].machine_id, "local");
        assert_eq!(groups[0].managed_by, "controller-1");
        assert_eq!(groups[0].destinations[0].machine_id, "local");
    }

    #[test]
    fn delegated_groups_are_saved_to_remote_config() {
        let temp = temp_dir("backend_delegated_persist");
        let config_path = temp.join("auto_sync.toml");
        let mut initial = AppConfig::default();
        initial.app.data_db = temp.join("state").join("auto_sync.sqlite");
        initial.app.log_dir = temp.join("logs");
        crate::core::config::save_config(&config_path, &initial).unwrap();

        let backend = Backend::new(config_path.clone(), 18765);
        let delegated = crate::core::config::SourceGroupConfig {
            id: "src_from_controller".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::from("/zfs"),
            add_directory: false,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: crate::core::config::SyncMode::Mirror,
            snapshot: crate::core::config::SnapshotConfig::default(),
            destinations: vec![crate::core::config::DestinationConfig {
                id: "dst_local".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/zfs_pool"),
                enabled: true,
                schedule: crate::core::config::ScheduleConfig::default(),
                sync: None,
            }],
        };

        backend
            .apply_delegated_source_groups(DelegatedSourceGroupsRequest {
                controller_id: "controller-1".to_string(),
                machines: Vec::new(),
                source_groups: vec![delegated],
            })
            .unwrap();

        let saved = crate::core::config::load_config(&config_path).unwrap();
        let source = saved
            .source_groups
            .iter()
            .find(|source| source.id == "src_from_controller")
            .unwrap();
        assert_eq!(source.managed_by, "controller-1");
        assert_eq!(source.machine_id, "local");
        assert_eq!(source.src, PathBuf::from("/zfs"));
    }

    #[test]
    fn delegated_config_collapses_self_references_and_prunes_self_machine() {
        let temp = temp_dir("backend_delegated_self");
        let config_path = temp.join("auto_sync.toml");
        let mut initial = AppConfig::default();
        initial.app.data_db = temp.join("state").join("auto_sync.sqlite");
        initial.app.log_dir = temp.join("logs");
        crate::core::config::save_config(&config_path, &initial).unwrap();

        let backend = Backend::new(config_path.clone(), 18765);
        // Controller pushes a machine list that includes an entry for THIS host
        // (loopback) plus a genuine remote peer, and a source whose source and
        // destination still reference the self-entry by id.
        let self_machine = MachineConfig {
            id: "selfhost".to_string(),
            alias_name: "selfhost".to_string(),
            name: "selfhost".to_string(),
            host: "127.0.0.1".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        };
        let remote_machine = MachineConfig {
            id: "peer".to_string(),
            host: "192.168.240.9".to_string(),
            ..self_machine.clone()
        };
        let delegated = crate::core::config::SourceGroupConfig {
            id: "src_self".to_string(),
            machine_id: "selfhost".to_string(),
            src: PathBuf::from("/zfs"),
            add_directory: false,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: crate::core::config::SyncMode::Mirror,
            snapshot: crate::core::config::SnapshotConfig::default(),
            destinations: vec![crate::core::config::DestinationConfig {
                id: "dst_self".to_string(),
                machine_id: "selfhost".to_string(),
                path: PathBuf::from("/zfs_pool"),
                enabled: true,
                schedule: crate::core::config::ScheduleConfig::default(),
                sync: None,
            }],
        };

        backend
            .apply_delegated_source_groups(DelegatedSourceGroupsRequest {
                controller_id: "controller-1".to_string(),
                machines: vec![self_machine, remote_machine],
                source_groups: vec![delegated],
            })
            .unwrap();

        let saved = crate::core::config::load_config(&config_path).unwrap();
        let source = saved
            .source_groups
            .iter()
            .find(|s| s.id == "src_self")
            .unwrap();
        // Self references collapsed to "local".
        assert_eq!(source.machine_id, "local");
        assert_eq!(source.destinations[0].machine_id, "local");
        // The self-entry was not added as a duplicate machine; the genuine
        // remote peer was kept.
        assert!(
            !saved.machines.iter().any(|m| m.id == "selfhost"),
            "self machine should be pruned"
        );
        assert!(
            saved.machines.iter().any(|m| m.id == "peer"),
            "remote peer should be retained"
        );
    }

    #[test]
    fn discovery_refreshes_stale_remote_metadata() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "nas".to_string(),
            alias_name: "nas".to_string(),
            name: "tiger".to_string(), // stale hostname from before a rename
            host: "192.168.2.247".to_string(),
            port: 18765,
            ssh_user: "olduser".to_string(), // stale; health advertises root
            ssh_port: 22,                    // stale default; health advertises 10022
            os: "linux".to_string(),
            install_dir: PathBuf::from("/old/dir"), // stale; health reports /opt/auto_sync
            enabled: true,
            manual: true,
        });
        let discovered = vec![MachineHealth {
            id: "local".to_string(),
            alias_name: String::new(),
            name: "nas".to_string(), // the machine reports its real hostname
            host: "192.168.2.247".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 10022,
            os: "linux".to_string(),
            install_dir: "/opt/auto_sync".to_string(),
            version: String::new(),
        }];

        let changed = refresh_machine_metadata_from_health(&mut cfg, &discovered);
        assert!(changed);
        let nas = cfg.machines.iter().find(|m| m.id == "nas").unwrap();
        assert_eq!(nas.name, "nas");
        assert_eq!(nas.install_dir, PathBuf::from("/opt/auto_sync"));
        assert_eq!(nas.ssh_user, "root", "ssh user preferred from discovery");
        assert_eq!(nas.ssh_port, 10022, "ssh port preferred from discovery");
        assert_eq!(nas.alias_name, "nas", "alias must be preserved");

        // Idempotent: a second pass with the same health makes no change.
        assert!(!refresh_machine_metadata_from_health(&mut cfg, &discovered));
    }

    #[test]
    fn rejects_source_path_change_after_destination_exists() {
        let temp = temp_dir("backend_locked_source_path");
        let config_path = temp.join("auto_sync.toml");

        let mut current = AppConfig::default();
        current.app.data_db = temp.join("state").join("auto_sync.sqlite");
        current.app.log_dir = temp.join("logs");
        current
            .source_groups
            .push(crate::core::config::SourceGroupConfig {
                id: "src_1".to_string(),
                machine_id: "local".to_string(),
                src: temp.join("src"),
                add_directory: true,
                managed_by: String::new(),
                excludes: Vec::new(),
                enabled: true,
                mode: crate::core::config::SyncMode::Mirror,
                snapshot: crate::core::config::SnapshotConfig::default(),
                destinations: vec![crate::core::config::DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: temp.join("dst"),
                    enabled: true,
                    schedule: crate::core::config::ScheduleConfig::default(),
                    sync: None,
                }],
            });
        crate::core::config::save_config(&config_path, &current).unwrap();

        let mut next = current.clone();
        next.source_groups[0].src = temp.join("other_src");

        let err = reject_locked_source_path_changes(&config_path, &next).unwrap_err();
        assert!(err.to_string().contains("source path is locked"));
    }

    #[test]
    fn resets_destination_offset_when_destination_path_changes() {
        let temp = temp_dir("backend_reset_destination_offset");
        let mut current = AppConfig::default();
        current.app.data_db = temp.join("state").join("auto_sync.sqlite");
        current.app.log_dir = temp.join("logs");
        current
            .source_groups
            .push(crate::core::config::SourceGroupConfig {
                id: "src_1".to_string(),
                machine_id: "local".to_string(),
                src: temp.join("src"),
                add_directory: true,
                managed_by: String::new(),
                excludes: Vec::new(),
                enabled: true,
                mode: crate::core::config::SyncMode::Mirror,
                snapshot: crate::core::config::SnapshotConfig::default(),
                destinations: vec![crate::core::config::DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: temp.join("dst_a"),
                    enabled: true,
                    schedule: crate::core::config::ScheduleConfig::default(),
                    sync: None,
                }],
            });

        let state = DbState::open(&current.app.data_db).unwrap();
        state.ensure_config(&current).unwrap();
        state.set_destination_target("src_1", "dst_1", 7).unwrap();
        state
            .upsert_destination_status("src_1", "dst_1", Some(7), "green", "verified")
            .unwrap();

        let mut next = current.clone();
        next.source_groups[0].destinations[0].path = temp.join("dst_b");
        reset_changed_destination_offsets(
            &state,
            &clean_config_for_save(&current),
            &clean_config_for_save(&next),
        )
        .unwrap();

        let offset = state.destination_offset("src_1", "dst_1").unwrap();
        assert_eq!(offset.target_cycle_id, None);
        assert_eq!(offset.last_completed_cycle_id, None);
        assert_eq!(offset.last_verified_cycle_id, None);
        assert_eq!(offset.status, "red");
        assert_eq!(offset.status_reason, "destination_path_changed");
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("auto_sync_{name}_{}_{}", std::process::id(), nanos));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
