use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub app: AppSection,
    #[serde(skip_serializing)]
    pub schedule: ScheduleConfig,
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
    pub enabled: bool,
    pub mode: SyncMode,
    pub destinations: Vec<DestinationConfig>,
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
            schedule: ScheduleConfig::default(),
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
            enabled: true,
            mode: SyncMode::Mirror,
            destinations: Vec::new(),
        }
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
        save_config(path, &cfg)?;
        return Ok(cfg);
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

pub fn save_config(path: &Path, cfg: &AppConfig) -> Result<()> {
    cfg.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(cfg).context("failed to serialize config")?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "failed to atomically replace {} with {}",
            path.display(),
            tmp.display()
        )
    })?;
    Ok(())
}

impl AppConfig {
    pub fn validate(&self) -> Result<()> {
        validate_schedule(&self.schedule)?;

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
            enabled: true,
            mode: SyncMode::Mirror,
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
                path: PathBuf::from("/data/src/backup"),
                enabled: true,
                schedule: ScheduleConfig::default(),
            }],
        });
        assert!(cfg.validate().is_err());
    }
}
