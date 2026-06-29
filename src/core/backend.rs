use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::core::config::{
    AppConfig, MachineConfig, clean_config_for_save, load_config, load_or_create_config,
    machine_matches_reference, save_config,
};
use crate::core::machines::{
    MachineHealth, MachineStatus, configure_tcp_connection_pool, discover_lan,
    encode_query_component, find_machine, local_health, machine_id_from_path, merge_discovered,
    remote_get_json,
};
use crate::core::progress::{
    ScanProgressView, TransferProgressView, configure_progress_file, current_scan_progress,
    current_transfer_progress,
};
use crate::core::state::{DestinationView, State as DbState};
use crate::core::sync::{
    SyncRequestMode, sync_all_now, sync_destination_now_with_mode, sync_source_now,
};

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
        let cfg = preserve_current_machines(&self.config_path, cfg);
        reject_locked_source_path_changes(&self.config_path, &cfg)?;
        let next = clean_config_for_save(&cfg);
        let cfg = save_config(&self.config_path, &cfg)?;
        apply_runtime_config(&cfg);
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        if let Some(current) = current.as_ref() {
            reset_changed_destination_offsets(&state_db, current, &next)?;
        }
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
        state_db.destination_views(&cfg)
    }

    pub fn runtime_status(&self) -> RuntimeStatus {
        RuntimeStatus {
            transfer: current_transfer_progress(),
            scan: current_scan_progress(),
            build: BuildInfo::current(),
        }
    }

    pub fn sync_now(&self) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_all_now(&cfg, &mut state_db)?;
        state_db.destination_views(&cfg)
    }

    pub fn sync_source_now(&self, source_id: &str) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_source_now(&cfg, &mut state_db, source_id)?;
        state_db.destination_views(&cfg)
    }

    pub fn sync_destination_now(
        &self,
        source_id: &str,
        destination_id: &str,
        mode: SyncRequestMode,
    ) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_destination_now_with_mode(&cfg, &mut state_db, source_id, destination_id, mode)?;
        state_db.destination_views(&cfg)
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
        let mut cache = self
            .machine_cache
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let (Some(status), Some(refreshed_at)) = (&cache.status, cache.refreshed_at) {
            if refreshed_at.elapsed() < min_interval {
                return Ok(status.clone());
            }
        }

        let cfg = load_or_create_config(&self.config_path)?;
        apply_runtime_config(&cfg);
        let discovered = discover_lan(Duration::from_millis(700))?;
        let status = merge_discovered(&cfg, discovered);
        cache.status = Some(status.clone());
        cache.refreshed_at = Some(Instant::now());
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
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub transfer: Option<TransferProgressView>,
    pub scan: Option<ScanProgressView>,
    pub build: BuildInfo,
}

#[derive(Debug, Clone, Serialize)]
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

fn apply_runtime_config(cfg: &AppConfig) {
    configure_tcp_connection_pool(cfg.app.tcp_connection_pool_size);
    configure_progress_file(&cfg.app.data_db);
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
