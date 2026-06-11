use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub app: AppSection,
    pub source_groups: Vec<SourceGroupConfig>,
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
    pub path: PathBuf,
    pub enabled: bool,
    pub schedule: ScheduleConfig,
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
            source_groups: Vec::new(),
            deploy: DeployConfig {
                targets: vec![DeployTarget {
                    id: "nas".to_string(),
                    host: "192.168.3.178".to_string(),
                    port: 10022,
                    user: "root".to_string(),
                    install_dir: PathBuf::from("/opt/auto_sync"),
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
            path: PathBuf::new(),
            enabled: true,
            schedule: ScheduleConfig::default(),
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
            install_dir: PathBuf::from("/opt/auto_sync"),
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
    for source in &mut cfg.source_groups {
        source.src = clean_path(&source.src);
        source.excludes = clean_excludes(&source.excludes);
        let mut destination_paths = HashSet::new();
        source.destinations.retain_mut(|dst| {
            dst.path = clean_path(&dst.path);
            if dst.path.as_os_str().is_empty() {
                return false;
            }
            destination_paths.insert(normalize_lossy(&dst.path))
        });
    }
    cfg.source_groups
        .retain(|source| !source.src.as_os_str().is_empty());
    cfg
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
            }
        }
        Ok(())
    }
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
            src: PathBuf::from("/data/src"),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
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
            src,
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
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
                    path: PathBuf::from(" /data/dst "),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    path: PathBuf::new(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
                DestinationConfig {
                    id: "dst_3".to_string(),
                    path: PathBuf::from("/data/dst"),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                },
            ],
        });
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_2".to_string(),
            src: PathBuf::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
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
