use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer, Serialize, de};

pub const DEFAULT_TCP_CONNECTION_POOL_SIZE: usize = 100;
pub const DEFAULT_TRANSFER_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_MAX_PARALLEL_TRANSFERS: usize = 16;
pub const DEFAULT_MODIFY_WINDOW_SECS: u64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub app: AppSection,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<MachineConfig>,
    pub source_groups: Vec<SourceGroupConfig>,
    /// Disks that should spin up only on a schedule (cold-backup HDDs). Any
    /// sync/compare/full touching one of a pool's `mount_roots` is deferred
    /// while that pool is outside its wake window, so the disk stays parked.
    /// Empty (the default) keeps every disk always-available — the feature is
    /// entirely dormant until a pool is configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standby_pools: Vec<StandbyPoolConfig>,
    #[serde(default, skip_serializing)]
    pub deploy: DeployConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSection {
    pub data_db: PathBuf,
    pub log_dir: PathBuf,
    pub status_log_interval_secs: u64,
    #[serde(
        default = "default_app_port",
        alias = "web_bind",
        deserialize_with = "deserialize_port"
    )]
    pub port: u16,
    pub tcp_connection_pool_size: usize,
    /// Shared secret for the machine-to-machine APIs (/api/transfer/*,
    /// delegated config pushes). Set the SAME value on every machine: peers
    /// then send it as a header and each machine rejects peer requests
    /// without it. Empty (the default) keeps the open LAN-trust behavior —
    /// anyone who can reach the port can write/delete under destination
    /// roots.
    pub peer_token: String,
    /// Preferred /24 prefix (with trailing dot, e.g. "192.168.2.") for the
    /// LAN-facing address: web bind, discovery self-report, self detection.
    /// Empty keeps the built-in default.
    pub preferred_subnet: String,
    pub sync: NativeSyncConfig,
    /// Start auto_sync automatically when the user logs in (desktop only).
    #[serde(default = "default_true")]
    pub autostart: bool,
    /// Closing the main window minimizes to the system tray instead of quitting
    /// (keeps the daemon running). Desktop only.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NativeSyncConfig {
    pub mirror: bool,
    pub checksum: bool,
    pub debug_logs: bool,
    pub transfer_timeout_secs: u64,
    pub bwlimit_kbps: u64,
    /// Number of files transferred concurrently to a destination. 0 = auto.
    pub max_parallel_transfers: usize,
    /// Files whose size matches and whose mtime differs by at most this many
    /// seconds are treated as identical (rsync-style modify-window). This keeps
    /// cross-platform timestamp granularity (Windows ↔ Linux/ZFS) from forcing
    /// endless re-transfers. Only used when checksum is disabled.
    pub modify_window_secs: u64,
    /// Use `zfs diff` against verified baseline snapshots wherever possible:
    /// engine incremental reconciles, manual Full (both sides diffed against
    /// their baselines, only the union of changed paths reconciled), and
    /// Compare. Requires the trees to live on local ZFS datasets with
    /// baselines established by a previous verified pass; anything else
    /// falls back to the full tree walks regardless of this flag. Disable
    /// per destination to force walk-based Full/Compare (e.g. when the
    /// baseline itself is suspect after a disk incident).
    pub zfs_diff: bool,
    /// fsync every received file (and its parent directory) before the atomic
    /// rename, for crash/power-loss durability. On by default: without it a
    /// crash after the rename returns but before write-back can leave a
    /// zero-length or stale destination file even though the sync reported
    /// success. Can be disabled for max throughput on sync filesystems where an
    /// fsync per file is costly; a backup re-verifies each cycle so the data is
    /// recoverable, but the published "verified" state would briefly be a lie.
    pub fsync: bool,
    /// Days a mirror-deleted entry stays in the destination's
    /// `.auto_sync_trash` before its per-cycle folder is reclaimed (age is
    /// measured from the folder's last write). 0 keeps the trash forever —
    /// note it then grows without bound, invisible to Compare.
    pub trash_keep_days: u64,
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

/// A cold-backup disk/pool that should stay parked and spin up only on a
/// schedule. See [`AppConfig::standby_pools`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StandbyPoolConfig {
    /// Display/reference name, e.g. "zfs" or "zfs_pool".
    pub name: String,
    /// Filesystem roots that live on this pool's disks. A task is gated when
    /// its source OR destination root is at/under any of these paths.
    pub mount_roots: Vec<PathBuf>,
    /// Physical block devices to `hdparm -Y` when the window closes (Linux,
    /// only when `active_spindown`). Use stable `/dev/disk/by-id/...` names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<String>,
    pub enabled: bool,
    /// Actively spin the devices down (`hdparm -Y`) at window close. When
    /// false the pool is only *gated* (tasks deferred) and spin-down is left
    /// to the OS idle timer (`hdparm -S`).
    pub active_spindown: bool,
    pub wake: WakeSchedule,
}

impl Default for StandbyPoolConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            mount_roots: Vec::new(),
            devices: Vec::new(),
            enabled: true,
            active_spindown: false,
            wake: WakeSchedule::default(),
        }
    }
}

/// When a standby pool is allowed to be awake. The window OPENS at the
/// scheduled weekday/time on every `every_weeks`-th week (counted from
/// `anchor_date`); it stays open until the pool's backlog drains and quiesces
/// (bounded by `max_window_minutes`), then the disk is parked again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WakeSchedule {
    /// 1 = every week, 4 = every 4th week, etc. (>=1).
    pub every_weeks: u32,
    /// Weekday the window opens on, e.g. "saturday".
    pub weekday: String,
    /// Local time the window opens, "HH:MM".
    pub time: String,
    /// ISO date (YYYY-MM-DD) that week-0 is counted from, so `every_weeks`
    /// cadence is stable across restarts. Any date on the intended cadence.
    pub anchor_date: String,
    /// Hard cap on how long the pool stays awake after opening, even if work
    /// has not drained (guards against a stuck task pinning the disk on).
    pub max_window_minutes: u64,
}

impl Default for WakeSchedule {
    fn default() -> Self {
        Self {
            every_weeks: 1,
            weekday: "saturday".to_string(),
            time: "10:00".to_string(),
            anchor_date: "2026-01-03".to_string(), // a Saturday
            max_window_minutes: 12 * 60,
        }
    }
}

/// File-collector configuration. Pulls files/directories from SSH hosts (via
/// the system `ssh`/`scp`) into a single local git repository, preserving each
/// remote path under a per-host local root, splitting oversized files so they
/// fit git hosting limits, and committing/pushing the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    /// Local git repository the pulled files are committed into. `git init` is
    /// run automatically if it is not yet a repo.
    pub git_dir: PathBuf,
    /// Path to an `ssh_config(5)`-style file (like `~/.ssh/config`) passed to
    /// `ssh`/`scp` with `-F`. Rewritten from `ssh_config` before every run so
    /// the on-disk file always matches what the UI shows.
    pub ssh_config_path: PathBuf,
    /// Contents of the ssh config file, edited from the Collector modal. Host
    /// aliases defined here can be referenced by `CollectorHost::ssh`.
    pub ssh_config: String,
    /// Files at least this many MiB are split into `<name>.autosplit.NNN`
    /// parts; the original is added to `.gitignore` and the parts committed.
    /// Kept under GitHub's 100 MiB hard limit by default. 0 disables splitting.
    pub split_threshold_mb: u64,
    /// Run `git add -A && git commit && git push` after a successful pull.
    pub auto_commit_push: bool,
    pub hosts: Vec<CollectorHost>,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            git_dir: PathBuf::new(),
            ssh_config_path: PathBuf::new(),
            ssh_config: String::new(),
            split_threshold_mb: 95,
            auto_commit_push: true,
            hosts: Vec::new(),
        }
    }
}

impl CollectorConfig {
    /// True when nothing has been configured — used to keep an empty
    /// `[collector]` table out of the serialized config.
    pub fn is_empty(&self) -> bool {
        self.git_dir.as_os_str().is_empty()
            && self.ssh_config_path.as_os_str().is_empty()
            && self.ssh_config.trim().is_empty()
            && self.hosts.is_empty()
    }
}

/// One SSH host the collector pulls from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectorHost {
    /// Friendly label (defaults to `ssh` when blank).
    pub name: String,
    /// ssh destination: either an alias defined in the ssh config, or a plain
    /// `user@host` / `host`.
    pub ssh: String,
    /// Local root that every remote path is reconstructed under. e.g. root
    /// `D:\share\linux\aws` + remote `/usr/local/x` => `…\aws\usr\local\x`.
    pub root: PathBuf,
    /// Absolute remote paths (files or directories) to pull.
    pub paths: Vec<String>,
    /// Optional shell command run over ssh to install the sftp-server when the
    /// remote lacks it (e.g. `apk add openssh-sftp-server`). Blank = skip.
    pub install_cmd: String,
    pub enabled: bool,
}

impl Default for CollectorHost {
    fn default() -> Self {
        Self {
            name: String::new(),
            ssh: String::new(),
            root: PathBuf::new(),
            paths: Vec::new(),
            install_cmd: String::new(),
            enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceGroupConfig {
    pub id: String,
    #[serde(skip_serializing_if = "is_local_machine")]
    pub machine_id: String,
    pub src: PathBuf,
    #[serde(default = "default_source_add_directory")]
    pub add_directory: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub managed_by: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub excludes: Vec<PathBuf>,
    pub enabled: bool,
    /// Display order in the UI source list (ascending). Purely cosmetic — it
    /// does not affect sync behaviour. The UI keeps these contiguous
    /// (0, 1, 2, ...) as the user drags sources to reorder.
    #[serde(default)]
    pub order: i64,
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
    /// Paused destinations receive no work: the scheduler assigns them no
    /// new targets, the engine holds their pending ones (kept, not dropped —
    /// resuming continues where the pause left off), and manual syncs are
    /// refused. Compare stays available (read-only).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    pub schedule: ScheduleConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<NativeSyncConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MachineConfig {
    pub id: String,
    pub alias_name: String,
    pub name: String,
    pub host: String,
    #[serde(
        default = "default_machine_port",
        alias = "web_port",
        deserialize_with = "deserialize_port"
    )]
    pub port: u16,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub os: String,
    pub install_dir: PathBuf,
    pub enabled: bool,
    pub manual: bool,
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
            standby_pools: Vec::new(),
            deploy: DeployConfig::default(),
        }
    }
}

impl Default for AppSection {
    fn default() -> Self {
        Self {
            data_db: PathBuf::from("conf/state/auto_sync.sqlite"),
            log_dir: PathBuf::from("logs"),
            status_log_interval_secs: 300,
            port: default_app_port(),
            tcp_connection_pool_size: DEFAULT_TCP_CONNECTION_POOL_SIZE,
            peer_token: String::new(),
            preferred_subnet: String::new(),
            sync: NativeSyncConfig::default(),
            autostart: true,
            close_to_tray: true,
        }
    }
}

impl Default for NativeSyncConfig {
    fn default() -> Self {
        Self {
            mirror: true,
            checksum: false,
            debug_logs: false,
            transfer_timeout_secs: DEFAULT_TRANSFER_TIMEOUT_SECS,
            bwlimit_kbps: 0,
            max_parallel_transfers: DEFAULT_MAX_PARALLEL_TRANSFERS,
            modify_window_secs: DEFAULT_MODIFY_WINDOW_SECS,
            zfs_diff: true,
            fsync: true,
            trash_keep_days: 30,
        }
    }
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            mode: ScheduleMode::Realtime,
            time: "19:00".to_string(),
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
            add_directory: default_source_add_directory(),
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
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
            paused: false,
            schedule: ScheduleConfig::default(),
            sync: None,
        }
    }
}

impl MachineConfig {
    pub fn local() -> Self {
        Self {
            id: "local".to_string(),
            alias_name: String::new(),
            name: local_hostname(),
            host: preferred_local_host(),
            port: default_machine_port(),
            ssh_user: String::new(),
            ssh_port: 22,
            os: std::env::consts::OS.to_string(),
            install_dir: default_install_dir_for_os(std::env::consts::OS),
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
            install_dir: PathBuf::from("/opt/auto_sync"),
        }
    }
}

/// Resolve which config file to use.
///
/// If the user passed `--config` explicitly we honour it exactly. Otherwise we
/// look for an existing `conf/auto_sync.toml`, first relative to the current
/// directory and then relative to the executable (so launching the binary from
/// `bin\` finds the repository's `conf\auto_sync.toml` one level up instead of a
/// stray `bin\conf\auto_sync.toml`). When nothing exists yet we fall back to the
/// current-directory relative path, which gets created on first run.
pub fn resolve_config_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let relative = Path::new("conf").join("auto_sync.toml");
    if relative.exists() {
        return relative;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // exe in `bin\`: the real config lives in the parent's `conf\`.
            if let Some(parent) = exe_dir.parent() {
                let candidate = parent.join("conf").join("auto_sync.toml");
                if candidate.exists() {
                    return candidate;
                }
            }
            let candidate = exe_dir.join("conf").join("auto_sync.toml");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    relative
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

/// Serializes every load→mutate→save sequence in this process. Six writers
/// (UI save, peer delegation push, machine add/remove, the discovery
/// thread's metadata refresh) each read-modify-write the whole file; an
/// unserialized interleave lets the later writer silently revert the earlier
/// one's changes with its stale snapshot.
pub fn config_write_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

pub fn save_config(path: &Path, cfg: &AppConfig) -> Result<AppConfig> {
    validate_unique_machine_ids(&cfg.machines)?;
    let cfg = clean_config_for_save(cfg);
    cfg.validate()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(&cfg).context("failed to serialize config")?;
    // Unique tmp name: concurrent writers sharing one fixed tmp path could
    // rename each other's half-written file into place.
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "toml.tmp.{}.{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
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
    if cfg.app.port == 0 {
        cfg.app.port = default_app_port();
    }
    clean_native_sync_config(&mut cfg.app.sync);
    merge_deploy_targets_into_machines(&mut cfg);
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
            if let Some(sync) = dst.sync.as_mut() {
                clean_native_sync_config(sync);
            }
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

fn merge_deploy_targets_into_machines(cfg: &mut AppConfig) {
    for target in &cfg.deploy.targets {
        let id = clean_id(&target.id);
        if id.is_empty() {
            continue;
        }
        if let Some(machine) = cfg
            .machines
            .iter_mut()
            .find(|machine| clean_id(&machine.id) == id)
        {
            if machine.host.trim().is_empty() {
                machine.host = target.host.trim().to_string();
            }
            if machine.ssh_user.trim().is_empty() {
                machine.ssh_user = target.user.trim().to_string();
            }
            if machine.ssh_port == 0 || machine.ssh_port == 22 {
                machine.ssh_port = target.port;
            }
            if machine.install_dir.as_os_str().is_empty() {
                machine.install_dir = target.install_dir.clone();
            }
            continue;
        }
        cfg.machines.push(MachineConfig {
            id: id.clone(),
            alias_name: String::new(),
            name: id,
            host: target.host.trim().to_string(),
            port: default_machine_port(),
            ssh_user: target.user.trim().to_string(),
            ssh_port: if target.port == 0 { 22 } else { target.port },
            os: String::new(),
            install_dir: target.install_dir.clone(),
            enabled: true,
            manual: true,
        });
    }
}

fn clean_native_sync_config(sync: &mut NativeSyncConfig) {
    if sync.transfer_timeout_secs == 0 {
        sync.transfer_timeout_secs = DEFAULT_TRANSFER_TIMEOUT_SECS;
    }
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
        machine.alias_name = clean_id(&machine.alias_name);
        machine.host = machine.host.trim().to_string();
        machine.ssh_user = machine.ssh_user.trim().to_string();
        machine.os = machine.os.trim().to_string();
        if machine.name.is_empty() {
            machine.name = machine.id.clone();
        }
        if machine.host.is_empty() {
            machine.host = preferred_local_host();
        }
        if machine.port == 0 {
            machine.port = default_machine_port();
        }
        if machine.ssh_port == 0 {
            machine.ssh_port = 22;
        }
        if machine.install_dir.as_os_str().is_empty() {
            machine.install_dir = default_install_dir_for_os(&machine.os);
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

    // Prefer the live hostname (already set by MachineConfig::local()). Only
    // fall back to a previously-stored name when hostname detection failed, so a
    // renamed host (e.g. "tiger" -> "nas") updates instead of persisting a stale
    // name, while a machine that genuinely can't resolve its hostname keeps a
    // sensible label.
    let configured_name = configured.name.trim();
    if is_placeholder_local_name(&local.name) && !is_placeholder_local_name(configured_name) {
        local.name = configured_name.to_string();
    }
    local.alias_name = clean_id(&configured.alias_name);
    if is_advertisable_host(&configured.host) {
        local.host = configured.host.trim().to_string();
    }
    if configured.port != 0 {
        local.port = configured.port;
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
    if !configured.install_dir.as_os_str().is_empty() {
        local.install_dir = configured.install_dir.clone();
    }
    local
}

fn default_app_port() -> u16 {
    18765
}

fn default_source_add_directory() -> bool {
    true
}

pub fn default_machine_port() -> u16 {
    18765
}

fn default_install_dir_for_os(os: &str) -> PathBuf {
    if os.eq_ignore_ascii_case("windows") {
        PathBuf::from("C:/auto_sync")
    } else {
        PathBuf::from("/opt/auto_sync")
    }
}

fn deserialize_port<'de, D>(deserializer: D) -> std::result::Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PortValue {
        Number(u16),
        Text(String),
    }

    match PortValue::deserialize(deserializer)? {
        PortValue::Number(port) => Ok(port),
        PortValue::Text(value) => parse_port_text(&value).map_err(de::Error::custom),
    }
}

fn parse_port_text(value: &str) -> Result<u16, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(default_app_port());
    }
    let port = trimmed
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or(trimmed)
        .trim()
        .parse::<u16>()
        .map_err(|err| format!("invalid port {trimmed}: {err}"))?;
    Ok(port)
}

fn is_placeholder_local_name(name: &str) -> bool {
    name.is_empty() || name == "This machine" || name.eq_ignore_ascii_case("local")
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
        validate_unique_machine_ids(&self.machines)?;
        let mut source_ids = HashSet::new();
        let machines = clean_machines(&self.machines);
        let machine_ids = machine_reference_set(&machines);
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
            if !machine_ids.contains(&source_machine.to_ascii_lowercase()) {
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
                if !machine_ids.contains(&dst_machine.to_ascii_lowercase()) {
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
            }
        }
        Ok(())
    }
}

fn validate_unique_machine_ids(machines: &[MachineConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for machine in machines {
        let id = clean_id(&machine.id);
        if id.is_empty() {
            continue;
        }
        if !seen.insert(id.clone()) {
            bail!("duplicate machine id: {id}");
        }
    }
    Ok(())
}

pub fn machine_id_or_local(value: &str) -> &str {
    if value.trim().is_empty() {
        "local"
    } else {
        value.trim()
    }
}

fn machine_reference_set(machines: &[MachineConfig]) -> HashSet<String> {
    let mut refs = HashSet::new();
    for machine in machines {
        insert_machine_reference(&mut refs, &machine.id);
        insert_machine_reference(&mut refs, &machine.alias_name);
        insert_machine_reference(&mut refs, &machine.host);
    }
    refs
}

fn insert_machine_reference(refs: &mut HashSet<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    refs.insert(value.to_ascii_lowercase());
}

pub fn machine_matches_reference(machine: &MachineConfig, value: &str) -> bool {
    let value = machine_id_or_local(value);
    let value_lower = value.to_ascii_lowercase();
    if value_lower == "local" {
        return machine.id == "local";
    }
    let alias = machine.alias_name.trim();
    if !alias.is_empty() && alias.eq_ignore_ascii_case(value) {
        return true;
    }
    if machine.id.eq_ignore_ascii_case(value) {
        return true;
    }
    machine.host.trim().eq_ignore_ascii_case(value)
}

/// Whether `machine_id` refers to the machine this process runs on. A
/// same-machine destination is sometimes labelled with the host's name (e.g.
/// "nas") instead of "local"; without recognizing that, the daemon would route
/// its own destination through the cross-machine transfer path (live source
/// reads, no ZFS snapshot, no zfs diff).
pub fn machine_is_local(cfg: &AppConfig, machine_id: &str) -> bool {
    let value = machine_id_or_local(machine_id);
    if value.eq_ignore_ascii_case("local") {
        return true;
    }
    if value.eq_ignore_ascii_case(local_hostname().trim()) {
        return true;
    }
    normalized_machines(cfg)
        .iter()
        .any(|machine| machine.id == "local" && machine_matches_reference(machine, value))
}

/// Whether a whole `MachineConfig` entry refers to the machine this process runs
/// on. Broader than [`machine_is_local`]: it also recognizes the entry by its
/// host being our LAN address, loopback, or hostname -- so a delegated peer that
/// carries our own LAN IP (e.g. the controller pushing the "nas" machine to the
/// NAS) is detected as ourselves and not treated as a separate remote peer.
pub fn machine_is_self(cfg: &AppConfig, machine: &MachineConfig) -> bool {
    if machine.id == "local" {
        return true;
    }
    for reference in [machine.id.as_str(), machine.alias_name.as_str()] {
        if !reference.trim().is_empty() && machine_is_local(cfg, reference) {
            return true;
        }
    }
    let host = machine.host.trim();
    !host.is_empty()
        && (host.eq_ignore_ascii_case("127.0.0.1")
            || host.eq_ignore_ascii_case("::1")
            || host.eq_ignore_ascii_case("localhost")
            || host.eq_ignore_ascii_case(preferred_local_host().trim())
            || host.eq_ignore_ascii_case(local_hostname().trim()))
}

/// Non-fatal configuration problems, surfaced in the UI status bar. Unlike
/// [`AppConfig::validate`] (which rejects the config outright), these flag likely
/// misconfigurations -- e.g. two machine entries sharing one IP -- so the user
/// can see and fix them without the daemon refusing to run.
pub fn config_warnings(cfg: &AppConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    let mut by_host: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for machine in &cfg.machines {
        let host = machine.host.trim().to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }
        by_host
            .entry(host)
            .or_default()
            .push(machine_display_name(machine));
    }
    for (host, mut names) in by_host {
        if names.len() > 1 {
            names.sort();
            names.dedup();
            if names.len() > 1 {
                warnings.push(format!(
                    "multiple machines share host {host}: {}",
                    names.join(", ")
                ));
            }
        }
    }

    let self_machines: Vec<String> = cfg
        .machines
        .iter()
        .filter(|machine| machine_is_self(cfg, machine))
        .map(machine_display_name)
        .collect();
    if self_machines.len() > 1 {
        warnings.push(format!(
            "multiple machine entries refer to this machine: {}",
            self_machines.join(", ")
        ));
    }

    warnings
}

pub fn machine_display_name(machine: &MachineConfig) -> String {
    let alias = machine.alias_name.trim();
    if !alias.is_empty() {
        return alias.to_string();
    }
    let host = machine.host.trim();
    if !host.is_empty() {
        return host.to_string();
    }
    let name = machine.name.trim();
    if !name.is_empty() {
        return name.to_string();
    }
    machine.id.clone()
}

pub fn normalized_machines(cfg: &AppConfig) -> Vec<MachineConfig> {
    clean_machines(&cfg.machines)
}

/// Preferred /24 for the LAN-facing address (web bind, discovery self-report,
/// machine_is_self). Configurable via `app.preferred_subnet` (e.g.
/// "10.0.7."); the previous hard-coded 192.168.2. stays as the default so
/// existing deployments behave identically.
pub fn configure_preferred_subnet(subnet: &str) {
    let subnet = subnet.trim();
    if subnet.is_empty() {
        return;
    }
    let mut slot = preferred_subnet_slot()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *slot = subnet.to_string();
}

fn preferred_subnet_slot() -> &'static std::sync::Mutex<String> {
    static SUBNET: OnceLock<std::sync::Mutex<String>> = OnceLock::new();
    SUBNET.get_or_init(|| std::sync::Mutex::new("192.168.2.".to_string()))
}

pub fn preferred_local_host() -> String {
    let subnet = preferred_subnet_slot()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let probe: Option<Ipv4Addr> = format!("{subnet}1").parse().ok();
    if let Some(probe) = probe {
        if let Some(ip) = detect_local_ip_for(probe).filter(|ip| ip.to_string().starts_with(&subnet))
        {
            return ip.to_string();
        }
    }
    if let Some(ip) = detect_local_ip_for(Ipv4Addr::new(8, 8, 8, 8)) {
        return ip.to_string();
    }
    "127.0.0.1".to_string()
}

pub fn local_hostname() -> String {
    static LOCAL_HOSTNAME: OnceLock<Option<String>> = OnceLock::new();
    LOCAL_HOSTNAME
        .get_or_init(detect_local_hostname)
        .clone()
        .unwrap_or_else(|| "This machine".to_string())
}

fn detect_local_hostname() -> Option<String> {
    for value in [
        std::env::var("COMPUTERNAME").ok(),
        std::env::var("HOSTNAME").ok(),
        std::fs::read_to_string("/etc/hostname").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    hostname_command_name()
}

fn hostname_command_name() -> Option<String> {
    let mut command = Command::new("hostname");
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command.output().ok().and_then(|output| {
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// The OS user this process runs as -- the account a peer would SSH in with.
/// Tries the usual environment variables first, then falls back to `whoami`
/// (which, unlike env vars, is populated even under systemd). Windows `whoami`
/// returns `DOMAIN\user`, so we keep only the user part.
pub fn process_user() -> Option<String> {
    for var in ["USER", "LOGNAME", "USERNAME"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    let mut command = Command::new("whoami");
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command.output().ok().and_then(|output| {
        let value = String::from_utf8(output.stdout).ok()?;
        let value = value.trim();
        let value = value.rsplit(['\\', '/']).next().unwrap_or(value).trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

fn detect_local_ip_for(peer: Ipv4Addr) -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddr::from((peer, 9))).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
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
            machine_id: "local".to_string(),
            src: PathBuf::from("/data/src"),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/data/src/backup"),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
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
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "bad".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
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
            add_directory: true,
            managed_by: String::new(),
            excludes: vec![
                PathBuf::from(" log "),
                PathBuf::from("cache/tmp"),
                PathBuf::from("log"),
                PathBuf::new(),
            ],
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![
                DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::from(" /data/dst "),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::new(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
                DestinationConfig {
                    id: "dst_3".to_string(),
                    machine_id: "local".to_string(),
                    path: PathBuf::from("/data/dst"),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
            ],
        });
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_2".to_string(),
            machine_id: "local".to_string(),
            src: PathBuf::new(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/unused"),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
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
    fn clean_config_for_save_replaces_local_name_placeholders() {
        for placeholder in ["This machine", "local"] {
            let mut cfg = AppConfig::default();
            cfg.machines[0].name = placeholder.to_string();

            let cleaned = clean_config_for_save(&cfg);

            assert_eq!(cleaned.machines[0].name, local_hostname());
        }
    }

    #[test]
    fn legacy_web_bind_and_web_port_deserialize_to_port() {
        let cfg: AppConfig = toml::from_str(
            r#"
[app]
web_bind = "0.0.0.0:18766"

[[machines]]
id = "nas"
name = "tiger"
host = "192.0.2.10"
web_port = 18767
"#,
        )
        .unwrap();

        assert_eq!(cfg.app.port, 18766);
        assert_eq!(cfg.machines[0].port, 18767);
    }

    #[test]
    fn legacy_source_defaults_to_adding_directory() {
        let cfg: AppConfig = toml::from_str(
            r#"
[[source_groups]]
id = "src_1"
src = "/zfs"
"#,
        )
        .unwrap();

        assert!(cfg.source_groups[0].add_directory);
    }

    #[test]
    fn clean_config_for_save_merges_legacy_deploy_targets_into_machines() {
        let mut cfg = AppConfig::default();
        cfg.machines.clear();
        cfg.deploy.targets.push(DeployTarget {
            id: "nas".to_string(),
            host: "192.0.2.10".to_string(),
            port: 10022,
            user: "root".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
        });

        let cleaned = clean_config_for_save(&cfg);

        let nas = cleaned
            .machines
            .iter()
            .find(|machine| machine.id == "nas")
            .unwrap();
        assert_eq!(nas.host, "192.0.2.10");
        assert_eq!(nas.port, 18765);
        assert_eq!(nas.ssh_user, "root");
        assert_eq!(nas.ssh_port, 10022);
        assert_eq!(nas.install_dir, PathBuf::from("/opt/auto_sync"));
    }

    #[test]
    fn rejects_duplicate_machine_ids() {
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "nas".to_string(),
            alias_name: "nas_a".to_string(),
            name: "nas-a".to_string(),
            host: "192.0.2.10".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 22,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });
        cfg.machines.push(MachineConfig {
            id: " nas ".to_string(),
            alias_name: "nas_b".to_string(),
            name: "nas-b".to_string(),
            host: "192.0.2.11".to_string(),
            port: 18765,
            ssh_user: "root".to_string(),
            ssh_port: 22,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate machine id: nas"));
    }

    #[test]
    fn save_config_rejects_duplicate_machine_ids_before_cleaning() {
        let temp = temp_dir("duplicate_machine_save");
        let path = temp.join("auto_sync.toml");
        let mut cfg = AppConfig::default();
        cfg.machines.push(MachineConfig {
            id: "local".to_string(),
            alias_name: String::new(),
            name: "another local".to_string(),
            host: "192.0.2.20".to_string(),
            port: 18765,
            ssh_user: String::new(),
            ssh_port: 22,
            os: "linux".to_string(),
            install_dir: PathBuf::from("/opt/auto_sync"),
            enabled: true,
            manual: true,
        });

        let err = save_config(&path, &cfg).unwrap_err();
        assert!(err.to_string().contains("duplicate machine id: local"));
        fs::remove_dir_all(temp).ok();
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
