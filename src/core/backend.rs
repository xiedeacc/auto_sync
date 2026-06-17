use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::core::config::{
    AppConfig, MachineConfig, load_config, load_or_create_config, save_config,
};
use crate::core::machines::{
    MachineHealth, MachineStatus, discover_lan, encode_query_component, find_machine, local_health,
    machine_id_from_path, machine_status, merge_discovered, remote_get_json,
};
use crate::core::state::{DestinationView, State as DbState};
use crate::core::sync::{sync_all_now, sync_destination_now, sync_source_now};

#[derive(Clone)]
pub struct Backend {
    config_path: Arc<PathBuf>,
    web_port: u16,
}

impl Backend {
    pub fn new(config_path: PathBuf, web_port: u16) -> Self {
        Self {
            config_path: Arc::new(config_path),
            web_port,
        }
    }

    pub fn config_path(&self) -> Arc<PathBuf> {
        self.config_path.clone()
    }

    pub fn web_port(&self) -> u16 {
        self.web_port
    }

    pub fn get_config(&self) -> Result<AppConfig> {
        load_or_create_config(&self.config_path)
    }

    pub fn save_config(&self, cfg: &AppConfig) -> Result<AppConfig> {
        let cfg = save_config(&self.config_path, cfg)?;
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        Ok(cfg)
    }

    pub fn health(&self) -> Result<MachineHealth> {
        let cfg = load_or_create_config(&self.config_path)?;
        Ok(local_health(&cfg, self.web_port))
    }

    pub fn machines(&self) -> Result<MachineStatus> {
        let cfg = load_or_create_config(&self.config_path)?;
        Ok(machine_status(&cfg))
    }

    pub fn discover_machines(&self) -> Result<MachineStatus> {
        let cfg = load_or_create_config(&self.config_path)?;
        let discovered = discover_lan(Duration::from_millis(700))?;
        Ok(merge_discovered(&cfg, discovered))
    }

    pub fn add_machine(&self, machine: MachineConfig) -> Result<AppConfig> {
        let mut cfg = load_or_create_config(&self.config_path)?;
        if let Some(existing) = cfg.machines.iter_mut().find(|item| item.id == machine.id) {
            *existing = machine;
        } else {
            cfg.machines.push(machine);
        }
        save_config(&self.config_path, &cfg)
    }

    pub fn remove_machine(&self, machine_id: &str) -> Result<AppConfig> {
        if machine_id == "local" {
            anyhow::bail!("local machine cannot be deleted");
        }
        let mut cfg = load_or_create_config(&self.config_path)?;
        cfg.machines.retain(|machine| machine.id != machine_id);
        save_config(&self.config_path, &cfg)
    }

    pub fn status(&self) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        let state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        state_db.destination_views(&cfg)
    }

    pub fn sync_now(&self) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_all_now(&cfg, &mut state_db)?;
        state_db.destination_views(&cfg)
    }

    pub fn sync_source_now(&self, source_id: &str) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_source_now(&cfg, &mut state_db, source_id)?;
        state_db.destination_views(&cfg)
    }

    pub fn sync_destination_now(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Vec<DestinationView>> {
        let cfg = load_config(&self.config_path)?;
        let mut state_db = DbState::open(&cfg.app.data_db)?;
        state_db.ensure_config(&cfg)?;
        sync_destination_now(&cfg, &mut state_db, source_id, destination_id)?;
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
