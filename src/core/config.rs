use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub app: AppSection,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<MachineConfig>,
    pub source_groups: Vec<SourceGroupConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sync_order: Vec<SyncOrderRule>,
    pub deploy: DeployConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSection {
    pub data_db: PathBuf,
    pub log_dir: PathBuf,
    pub status_log_interval_secs: u64,
    pub web_bind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    pub mode: ScheduleMode,
    pub time: String,
    pub timezone: String,
    pub weekday: Option<String>,
    pub sync_current_cycle_manually: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleMode {
    Realtime,
    Daily,
    Weekly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceGroupConfig {
    pub id: String,
    #[serde(skip_serializing_if = "is_local_machine")]
    pub machine_id: String,
    pub src: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<PathBuf>,
    pub enabled: bool,
    pub mode: SyncMode,
    pub snapshot: SnapshotConfig,
    pub destinations: Vec<DestinationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotConfig {
    pub backend: SnapshotBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_in_dataset: Option<PathBuf>,
    pub prefix: String,
    pub reconcile_interval_secs: u64,
    pub keep_extra_cycles: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SnapshotBackend {
    Auto,
    Manifest,
    Zfs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncMode {
    Mirror,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DestinationConfig {
    pub id: String,
    #[serde(skip_serializing_if = "is_local_machine")]
    pub machine_id: String,
    pub path: PathBuf,
    pub enabled: bool,
    pub schedule: ScheduleConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MachineConfig {
    pub id: String,
    pub name: String,
    pub host: String,
    pub web_port: u16,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub os: String,
    pub enabled: bool,
    pub manual: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq, Hash)]
#[serde(default)]
pub struct SyncTaskRef {
    pub source_id: String,
    pub destination_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SyncOrderRule {
    pub before: SyncTaskRef,
    pub after: SyncTaskRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct DeployConfig {
    pub targets: Vec<DeployTarget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DeployTarget {
    pub id: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub install_dir: PathBuf,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            app: AppSection::default(),
            machines: vec![MachineConfig::local()],
            source_groups: Vec::new(),
            sync_order: Vec::new(),
            deploy: DeployConfig {
                targets: vec![DeployTarget {
                    id: "nas".to_string(),
                    host: "192.168.2.247".to_string(),
                    port: 10022,
                    user: "root".to_string(),
                    install_dir: PathBuf::from("/usr/local/auto_sync"),
                }],
            },
        }
    }
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            data_db: PathBuf::from("conf/state/auto_sync.sqlite"),
            log_dir: PathBuf::from("logs"),
            status_log_interval_secs: 300,
            web_bind: "0.0.0.0:18765".to_string(),
        }
    }
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            mode: ScheduleMode::Realtime,
            time: "02:00".to_string(),
            timezone: "local".to_string(),
            weekday: Some("monday".to_string()),
            sync_current_cycle_manually: false,
        }
    }
}

impl Default for ScheduleMode {
    fn default() -> Self {
        Self::Realtime
    }
}

impl Default for SourceGroupConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            machine_id: "local".to_string(),
            src: PathBuf::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: Vec::new(),
        }
    }
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            backend: SnapshotBackend::Auto,
            dataset: None,
            path_in_dataset: None,
            prefix: "auto_sync".to_string(),
            reconcile_interval_secs: 900,
            keep_extra_cycles: 2,
        }
    }
}

impl Default for SnapshotBackend {
    fn default() -> Self {
        Self::Auto
    }
}

impl Default for SyncMode {
    fn default() -> Self {
        Self::Mirror
    }
}

impl Default for DestinationConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            machine_id: "local".to_string(),
            path: PathBuf::new(),
            enabled: true,
            schedule: ScheduleConfig::default(),
        }
    }
}

impl MachineConfig {
    pub fn local() -> Self {
        Self {
            id: "local".to_string(),
            name: "This machine".to_string(),
            host: preferred_local_host(),
            web_port: 18765,
            ssh_user: String::new(),
            ssh_port: 22,
            os: std::env::consts::OS.to_string(),
            enabled: true,
            manual: true,
        }
    }
}

impl Default for DeployTarget {
    fn default() -> Self {
        Self {
            id: String::new(),
            host: String::new(),
            port: 22,
            user: "root".to_string(),
            install_dir: PathBuf::from("/usr/local/auto_sync"),
        }
    }
}

pub fn load_or_create_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        let cfg = AppConfig::default();
        return save_config(path, &cfg);
    }
    load_config(path)
}

pub fn load_config(path: &Path) -> Result<AppConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    let cfg: AppConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config {}", path.display()))?;
    cfg.validate()?;
    Ok(cfg)
}

pub fn save_config(path: &Path, cfg: &AppConfig) -> Result<AppConfig> {
    let cfg = clean_config_for_save(cfg);
    cfg.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(&cfg).context("failed to serialize config")?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(cfg)
}

pub fn clean_config_for_save(cfg: &AppConfig) -> AppConfig {
    let mut cfg = cfg.clone();
    cfg.machines = clean_machines(&cfg.machines);
    for source in &mut cfg.source_groups {
        if source.machine_id.trim().is_empty() {
            source.machine_id = "local".to_string();
        }
        source.src = clean_path(&source.src);
        source.excludes = clean_excludes(&source.excludes);
        let mut destination_paths = HashSet::new();
        source.destinations.retain_mut(|dst| {
            if dst.machine_id.trim().is_empty() {
                dst.machine_id = "local".to_string();
            }
            dst.path = clean_path(&dst.path);
            if dst.path.as_os_str().is_empty() {
                return false;
            }
            destination_paths.insert(normalize_lossy(&dst.path))
        });
    }
    cfg.source_groups
        .retain(|source| !source.src.as_os_str().is_empty());
    let task_ids = sync_task_ids(&cfg);
    let mut order_edges = HashSet::new();
    cfg.sync_order.retain(|rule| {
        let before = sync_task_key(&rule.before);
        let after = sync_task_key(&rule.after);
        before != after
            && task_ids.contains(&before)
            && task_ids.contains(&after)
            && order_edges.insert((before, after))
    });
    cfg
}

fn clean_machines(machines: &[MachineConfig]) -> Vec<MachineConfig> {
    let local = local_machine_from_config(machines);
    let mut seen = HashSet::new();
    let mut cleaned = Vec::new();
    for machine in std::iter::once(&local).chain(
        machines
            .iter()
            .filter(|machine| clean_id(&machine.id) != "local"),
    ) {
        let mut machine = machine.clone();
        machine.id = clean_id(&machine.id);
        if machine.id.is_empty() || !seen.insert(machine.id.clone()) {
            continue;
        }
        machine.name = machine.name.trim().to_string();
        machine.host = machine.host.trim().to_string();
        machine.ssh_user = machine.ssh_user.trim().to_string();
        machine.os = machine.os.trim().to_string();
        if machine.name.is_empty() {
            machine.name = machine.id.clone();
        }
        if machine.host.is_empty() {
            machine.host = preferred_local_host();
        }
        if machine.web_port == 0 {
            machine.web_port = 18765;
        }
        if machine.ssh_port == 0 {
            machine.ssh_port = 22;
        }
        cleaned.push(machine);
    }
    cleaned
}

fn local_machine_from_config(machines: &[MachineConfig]) -> MachineConfig {
    let mut local = MachineConfig::local();
    let Some(configured) = machines
        .iter()
        .find(|machine| clean_id(&machine.id) == "local")
    else {
        return local;
    };

    if !configured.name.trim().is_empty() {
        local.name = configured.name.trim().to_string();
    }
    if is_advertisable_host(&configured.host) {
        local.host = configured.host.trim().to_string();
    }
    if configured.web_port != 0 {
        local.web_port = configured.web_port;
    }
    if configured.ssh_port != 0 {
        local.ssh_port = configured.ssh_port;
    }
    if !configured.ssh_user.trim().is_empty() {
        local.ssh_user = configured.ssh_user.trim().to_string();
    }
    if !configured.os.trim().is_empty() {
        local.os = configured.os.trim().to_string();
    }
    local
}

fn is_advertisable_host(host: &str) -> bool {
    let host = host.trim();
    !host.is_empty()
        && host != "0.0.0.0"
        && host != "::"
        && host != "localhost"
        && !host.starts_with("127.")
}

fn clean_id(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn is_local_machine(value: &str) -> bool {
    value.is_empty() || value == "local"
}

fn clean_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let trimmed = text.trim();
    if trimmed.is_empty() {
        PathBuf::new()
    } else if trimmed == text.as_ref() {
        path.to_path_buf()
    } else {
        PathBuf::from(trimmed)
    }
}

fn clean_excludes(excludes: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut cleaned = Vec::new();
    for path in excludes {
        let path = clean_path(path);
        if path.as_os_str().is_empty() {
            continue;
        }
        let path = normalize_lossy(&path);
        if seen.insert(path.clone()) {
            cleaned.push(path);
        }
    }
    cleaned
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        let mut source_ids = HashSet::new();
        let mut task_ids = HashSet::new();
        let machine_ids: HashSet<String> = clean_machines(&self.machines)
            .into_iter()
            .map(|machine| machine.id)
            .collect();
        for source in &self.source_groups {
            if source.id.trim().is_empty() {
                bail!("source group id cannot be empty");
            }
            if !source_ids.insert(source.id.clone()) {
                bail!("duplicate source group id: {}", source.id);
            }
            if source.src.as_os_str().is_empty() {
                bail!("source {} has empty src path", source.id);
            }
            let source_machine = machine_id_or_local(&source.machine_id);
            if !machine_ids.contains(source_machine) {
                bail!(
                    "source {} references unknown machine {}",
                    source.id,
                    source_machine
                );
            }
            if source.snapshot.prefix.trim().is_empty() {
                bail!("source {} has empty snapshot prefix", source.id);
            }
            if source.snapshot.reconcile_interval_secs == 0 {
                bail!("source {} has zero reconcile interval", source.id);
            }
            for exclude in &source.excludes {
                validate_exclude_path(&source.id, exclude)?;
            }

            let mut dst_ids = HashSet::new();
            for dst in &source.destinations {
                if dst.id.trim().is_empty() {
                    bail!("source {} has destination with empty id", source.id);
                }
                if !dst_ids.insert(dst.id.clone()) {
                    bail!(
                        "source {} has duplicate destination id {}",
                        source.id,
                        dst.id
                    );
                }
                if dst.path.as_os_str().is_empty() {
                    bail!("destination {} has empty path", dst.id);
                }
                let dst_machine = machine_id_or_local(&dst.machine_id);
                if !machine_ids.contains(dst_machine) {
                    bail!(
                        "destination {} references unknown machine {}",
                        dst.id,
                        dst_machine
                    );
                }
                validate_schedule(&dst.schedule)
                    .with_context(|| format!("destination {} has invalid schedule", dst.id))?;
                if existing_directory_source_to_file_destination(source, dst) {
                    bail!(
                        "source {} is a directory, so destination {} must be a directory",
                        source.id,
                        dst.id
                    );
                }
                if path_has_prefix(&dst.path, &source.src) {
                    bail!(
                        "destination {} ({}) must not be inside source {} ({})",
                        dst.id,
                        dst.path.display(),
                        source.id,
                        source.src.display()
                    );
                }
                task_ids.insert(sync_task_key_parts(&source.id, &dst.id));
            }
        }
        validate_sync_order(&self.sync_order, &task_ids)?;
        Ok(())
    }
}

pub fn machine_id_or_local(value: &str) -> &str {
    if value.trim().is_empty() {
        "local"
    } else {
        value.trim()
    }
}

pub fn normalized_machines(cfg: &AppConfig) -> Vec<MachineConfig> {
    clean_machines(&cfg.machines)
}

pub fn preferred_local_host() -> String {
    if let Some(ip) = detect_local_ip_for(Ipv4Addr::new(192, 168, 2, 1))
        .filter(|ip| ip.octets()[0..3] == [192, 168, 2])
    {
        return ip.to_string();
    }
    if let Some(ip) = detect_local_ip_for(Ipv4Addr::new(8, 8, 8, 8)) {
        return ip.to_string();
    }
    "127.0.0.1".to_string()
}

fn detect_local_ip_for(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddr::from((peer, 9))).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

fn sync_task_ids(cfg: &AppConfig) -> HashSet<String> {
    cfg.source_groups
        .iter()
        .flat_map(|source| {
            source
                .destinations
                .iter()
                .map(|dst| sync_task_key_parts(&source.id, &dst.id))
        })
        .collect()
}

fn sync_task_key(task: &SyncTaskRef) -> String {
    sync_task_key_parts(&task.source_id, &task.destination_id)
}

fn sync_task_key_parts(source_id: &str, destination_id: &str) -> String {
    format!("{source_id}:{destination_id}")
}

fn validate_sync_order(rules: &[SyncOrderRule], task_ids: &HashSet<String>) -> Result<()> {
    let mut graph: HashMap<String, Vec<String>> = HashMap::new();
    let mut indegree: HashMap<String, usize> = HashMap::new();
    let mut edges = HashSet::new();

    for rule in rules {
        let before = sync_task_key(&rule.before);
        let after = sync_task_key(&rule.after);
        if !task_ids.contains(&before) {
            bail!("sync order references unknown task: {before}");
        }
        if !task_ids.contains(&after) {
            bail!("sync order references unknown task: {after}");
        }
        if before == after {
            bail!("sync order task cannot depend on itself: {before}");
        }
        if !edges.insert((before.clone(), after.clone())) {
            continue;
        }
        graph.entry(before.clone()).or_default().push(after.clone());
        graph.entry(after.clone()).or_default();
        indegree.entry(before).or_insert(0);
        *indegree.entry(after).or_insert(0) += 1;
    }

    let mut queue: VecDeque<String> = indegree
        .iter()
        .filter_map(|(task, count)| (*count == 0).then(|| task.clone()))
        .collect();
    let mut visited = 0_usize;
    while let Some(task) = queue.pop_front() {
        visited += 1;
        for next in graph.get(&task).into_iter().flatten() {
            let Some(count) = indegree.get_mut(next) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                queue.push_back(next.clone());
            }
        }
    }

    if visited != indegree.len() {
        bail!("sync order contains a cycle");
    }
    Ok(())
}

fn validate_exclude_path(source_id: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("source {source_id} has empty exclude path");
    }
    if path.is_absolute() {
        bail!(
            "source {source_id} exclude path must be relative: {}",
            path.display()
        );
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "source {source_id} exclude path must not contain parent components: {}",
            path.display()
        );
    }
    Ok(())
}

fn existing_directory_source_to_file_destination(
    source: &SourceGroupConfig,
    dst: &DestinationConfig,
) -> bool {
    let Ok(source_meta) = fs::symlink_metadata(&source.src) else {
        return false;
    };
    let Ok(dst_meta) = fs::symlink_metadata(&dst.path) else {
        return false;
    };
    source_meta.is_dir() && !dst_meta.is_dir()
}

pub fn validate_schedule(schedule: &ScheduleConfig) -> Result<()> {
    match schedule.mode {
        ScheduleMode::Realtime => Ok(()),
        ScheduleMode::Daily | ScheduleMode::Weekly => {
            parse_schedule_time(&schedule.time)?;
            Ok(())
        }
    }
}

pub fn parse_schedule_time(value: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<&str> = value.split(':').collect();
    if parts.len() != 2 && parts.len() != 3 {
        bail!("schedule time must use HH:MM");
    }
    let hour = parts[0].parse::<u32>().context("invalid schedule hour")?;
    let minute = parts[1].parse::<u32>().context("invalid schedule minute")?;
    let second = if parts.len() == 3 {
        parts[2].parse::<u32>().context("invalid schedule second")?
    } else {
        0
    };
    if hour > 23 || minute > 59 || second != 0 {
        return Err(anyhow!("schedule time out of range: {value}"));
    }
    Ok((hour, minute, second))
}

fn path_has_prefix(path: &Path, prefix: &Path) -> bool {
    let path = normalize_lossy(path);
    let prefix = normalize_lossy(prefix);
    path != prefix && path.starts_with(&prefix)
}

fn normalize_lossy(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        out.push(part.as_os_str());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_schedule_time() {
        assert_eq!(parse_schedule_time("02:03").unwrap(), (2, 3, 0));
        assert_eq!(parse_schedule_time("02:03:00").unwrap(), (2, 3, 0));
    }

    #[test]
    fn rejects_invalid_schedule_time() {
        assert!(parse_schedule_time("25:00").is_err());
        assert!(parse_schedule_time("02:00:01").is_err());
        assert!(parse_schedule_time("02").is_err());
    }

    #[test]
    fn rejects_destination_inside_source() {
        let mut cfg = AppConfig::default();
        cfg.source_groups.push(SourceGroupConfig {
            id: "main".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::from("/data/src"),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/data/src/backup"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_existing_directory_source_to_file_destination() {
        let temp = temp_dir("dir_to_file_config");
        let src = temp.join("src");
        let dst = temp.join("dst-file");
        fs::create_dir_all(&src).unwrap();
        fs::write(&dst, b"old").unwrap();

        let mut cfg = AppConfig::default();
        cfg.source_groups.push(SourceGroupConfig {
            id: "main".to_string(),
            machine_id: "local".to_string(),
            src,
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        assert!(cfg.validate().is_err());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn clean_config_for_save_drops_empty_sources_and_destinations() {
        let mut cfg = AppConfig::default();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::from(" /data/src "),
            excludes: vec![
                PathBuf::from(" log "),
                PathBuf::from("cache/tmp"),
                PathBuf::from("log"),
                PathBuf::new(),
            ],
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![
                DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::from(" /data/dst "),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::new(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
                DestinationConfig {
                    id: "dst_3".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::from("/data/dst"),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
            ],
        });
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_2".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/unused"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });

        let cleaned = clean_config_for_save(&cfg);

        assert_eq!(cleaned.source_groups.len(), 1);
        assert_eq!(cleaned.source_groups[0].src, PathBuf::from("/data/src"));
        assert_eq!(
            cleaned.source_groups[0].excludes,
            vec![PathBuf::from("log"), PathBuf::from("cache/tmp")]
        );
        assert_eq!(cleaned.source_groups[0].destinations.len(), 1);
        assert_eq!(
            cleaned.source_groups[0].destinations[0].path,
            PathBuf::from("/data/dst")
        );
    }

    #[test]
    fn validates_sync_order_dag() {
        let mut cfg = config_with_two_tasks();
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "src_1".to_string(),
                destination_id: "dst_1".to_string(),
            },
            after: SyncTaskRef {
                source_id: "src_2".to_string(),
                destination_id: "dst_1".to_string(),
            },
        });

        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_sync_order_cycle() {
        let mut cfg = config_with_two_tasks();
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "src_1".to_string(),
                destination_id: "dst_1".to_string(),
            },
            after: SyncTaskRef {
                source_id: "src_2".to_string(),
                destination_id: "dst_1".to_string(),
            },
        });
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "src_2".to_string(),
                destination_id: "dst_1".to_string(),
            },
            after: SyncTaskRef {
                source_id: "src_1".to_string(),
                destination_id: "dst_1".to_string(),
            },
        });

        assert!(cfg.validate().is_err());
    }

    #[test]
    fn clean_config_for_save_drops_stale_sync_order_rules() {
        let mut cfg = config_with_two_tasks();
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "src_1".to_string(),
                destination_id: "dst_1".to_string(),
            },
            after: SyncTaskRef {
                source_id: "missing".to_string(),
                destination_id: "dst_1".to_string(),
            },
        });
        cfg.sync_order.push(SyncOrderRule {
            before: SyncTaskRef {
                source_id: "src_1".to_string(),
                destination_id: "dst_1".to_string(),
            },
            after: SyncTaskRef {
                source_id: "src_2".to_string(),
                destination_id: "dst_1".to_string(),
            },
        });

        let cleaned = clean_config_for_save(&cfg);

        assert_eq!(cleaned.sync_order.len(), 1);
        assert_eq!(cleaned.sync_order[0].before.source_id, "src_1");
        assert_eq!(cleaned.sync_order[0].after.source_id, "src_2");
    }

    fn config_with_two_tasks() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::from("/data/src_1"),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/data/dst_1"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_2".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::from("/data/src_2"),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/data/dst_2"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        cfg
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("auto_sync_{name}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
