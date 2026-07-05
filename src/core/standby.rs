//! Cold-backup disk standby: gate sync/compare/full work so a pool's disks
//! stay parked outside a scheduled wake window, and spin them down when the
//! window closes.
//!
//! The engine calls [`gate_for_roots`] before touching a (source, dest) pair:
//! if any [`StandbyPoolConfig`] whose `mount_roots` cover either root is
//! outside its wake window, the task is deferred with a reason. When a pool IS
//! awake, [`verify_pool_mounted`] guards against the catastrophic case where a
//! pool root exists as a bare directory because the pool is not actually
//! imported — syncing then would read an empty tree and mirror-delete the
//! backup. The daemon drives [`StandbyManager`] each tick to open/close windows
//! and issue `hdparm -Y` on close.
//!
//! Entirely dormant until [`AppConfig::standby_pools`] is non-empty.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use anyhow::{Result, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone};
use tracing::{info, warn};

use crate::core::config::{StandbyPoolConfig, WakeSchedule};

/// Why a task cannot run right now because of standby (for status/task reason).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Pool is parked; the value is a short human reason incl. next wake.
    Asleep { pool: String, reason: String },
    /// Pool should be awake but its root is not a real mount — refuse to sync.
    NotMounted { pool: String, reason: String },
}

impl Gate {
    /// Short status token stored on the destination row / task reason.
    pub fn status_reason(&self) -> String {
        match self {
            Gate::Asleep { reason, .. } => reason.clone(),
            Gate::NotMounted { reason, .. } => reason.clone(),
        }
    }

    pub fn pool(&self) -> &str {
        match self {
            Gate::Asleep { pool, .. } | Gate::NotMounted { pool, .. } => pool,
        }
    }
}

/// True when `path` is at or under `root`.
fn path_under(root: &Path, path: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Does this pool gate the given filesystem path (is the path on its disks)?
pub fn pool_covers_path(pool: &StandbyPoolConfig, path: &Path) -> bool {
    pool.mount_roots.iter().any(|root| path_under(root, path))
}

/// Evaluate standby for a task touching `roots` (source root + dest root, plus
/// any others). Returns the first blocking [`Gate`], or `None` to proceed.
///
/// `now` is injected so this is deterministic and unit-testable.
pub fn gate_for_roots(
    pools: &[StandbyPoolConfig],
    roots: &[&Path],
    now: DateTime<Local>,
) -> Result<Option<Gate>> {
    for pool in pools.iter().filter(|p| p.enabled) {
        let touched = roots.iter().any(|r| pool_covers_path(pool, r));
        if !touched {
            continue;
        }
        if !is_within_wake_window(now, &pool.wake)? {
            let next = next_wake_start_after(now, &pool.wake)
                .map(|t| t.format("%a %Y-%m-%d %H:%M").to_string())
                .unwrap_or_else(|_| "?".to_string());
            return Ok(Some(Gate::Asleep {
                pool: pool.name.clone(),
                reason: format!("disk {} in standby until {next}", pool.name),
            }));
        }
        // Awake: the pool's disks must genuinely be mounted, or a "sync" would
        // read an empty tree and delete the whole backup.
        if let Err(err) = verify_pool_mounted(pool) {
            return Ok(Some(Gate::NotMounted {
                pool: pool.name.clone(),
                reason: format!("disk {} not mounted: {err}", pool.name),
            }));
        }
    }
    Ok(None)
}

/// Each `mount_root` must be a real mount point (distinct device id from its
/// parent), proving the pool is imported rather than a bare stub directory.
pub fn verify_pool_mounted(pool: &StandbyPoolConfig) -> Result<()> {
    for root in &pool.mount_roots {
        if !is_mount_point(root)? {
            bail!("{} is not a mount point (pool not imported?)", root.display());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_mount_point(root: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let Some(parent) = root.parent() else {
        return Ok(true); // filesystem root
    };
    let here = std::fs::metadata(root)
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", root.display()))?;
    let up = std::fs::metadata(parent)
        .map_err(|e| anyhow::anyhow!("stat {}: {e}", parent.display()))?;
    Ok(here.dev() != up.dev())
}

#[cfg(not(unix))]
fn is_mount_point(root: &Path) -> Result<bool> {
    // Non-unix has no cheap st_dev mount test; treat existence as good enough
    // (standby pools are a Linux/NAS feature in practice).
    Ok(root.exists())
}

/// The next moment a wake window opens strictly after `after`.
pub fn next_wake_start_after(
    after: DateTime<Local>,
    wake: &WakeSchedule,
) -> Result<DateTime<Local>> {
    let anchor = parse_anchor(wake)?;
    let wanted = parse_weekday(&wake.weekday)?;
    let time = parse_time(&wake.time)?;
    let every = wake.every_weeks.max(1) as i64;
    let mut date = after.date_naive();
    for _ in 0..(every * 7 + 8) {
        if date.weekday().num_days_from_monday() == wanted && week_on_cadence(anchor, date, every) {
            let start = local_dt(date, time)?;
            if start > after {
                return Ok(start);
            }
        }
        date = date.succ_opt().ok_or_else(|| anyhow::anyhow!("date overflow"))?;
    }
    bail!("could not find next wake within horizon")
}

/// Is `now` inside an open wake window? The window opens at the scheduled
/// weekday/time on a cadence week and lasts `max_window_minutes`.
pub fn is_within_wake_window(now: DateTime<Local>, wake: &WakeSchedule) -> Result<bool> {
    let anchor = parse_anchor(wake)?;
    let wanted = parse_weekday(&wake.weekday)?;
    let time = parse_time(&wake.time)?;
    let every = wake.every_weeks.max(1) as i64;
    let window = Duration::minutes(wake.max_window_minutes.max(1) as i64);
    // Scan back far enough to find the most recent window start.
    let mut date = now.date_naive();
    for _ in 0..(every * 7 + 8) {
        if date.weekday().num_days_from_monday() == wanted && week_on_cadence(anchor, date, every) {
            let start = local_dt(date, time)?;
            if start <= now && now < start + window {
                return Ok(true);
            }
            if start <= now {
                // Most recent start is before the window end check above; older
                // starts are even further back, so we can stop.
                return Ok(false);
            }
        }
        date = match date.pred_opt() {
            Some(d) => d,
            None => break,
        };
    }
    Ok(false)
}

fn week_on_cadence(anchor: NaiveDate, date: NaiveDate, every: i64) -> bool {
    let days = (date - anchor).num_days();
    // Same weekday as a cadence week: days is a multiple of 7 off the anchor's
    // weekday only when aligned; use floor-division week index.
    let week = days.div_euclid(7);
    week.rem_euclid(every) == 0
}

fn parse_anchor(wake: &WakeSchedule) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(&wake.anchor_date, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid anchor_date {}: {e}", wake.anchor_date))
}

fn parse_time(value: &str) -> Result<NaiveTime> {
    NaiveTime::parse_from_str(value, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M:%S"))
        .map_err(|e| anyhow::anyhow!("invalid time {value}: {e}"))
}

/// Weekday as days-from-monday (0=Mon..6=Sun), matching chrono's numbering.
fn parse_weekday(value: &str) -> Result<u32> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "mon" | "monday" => 0,
        "tue" | "tuesday" => 1,
        "wed" | "wednesday" => 2,
        "thu" | "thursday" => 3,
        "fri" | "friday" => 4,
        "sat" | "saturday" => 5,
        "sun" | "sunday" => 6,
        other => bail!("invalid weekday: {other}"),
    })
}

fn local_dt(date: NaiveDate, time: NaiveTime) -> Result<DateTime<Local>> {
    Local
        .from_local_datetime(&date.and_time(time))
        .single()
        .or_else(|| Local.from_local_datetime(&date.and_time(time)).earliest())
        .ok_or_else(|| anyhow::anyhow!("unresolvable local time"))
}

/// Park a pool's physical devices with `hdparm -Y`. Best-effort: logs and
/// continues on error (a device may already be asleep or busy).
pub fn spin_down_devices(pool: &StandbyPoolConfig) {
    if !pool.active_spindown {
        return;
    }
    for dev in &pool.devices {
        #[cfg(target_os = "linux")]
        {
            match std::process::Command::new("hdparm").arg("-Y").arg(dev).status() {
                Ok(s) if s.success() => info!(pool = pool.name, device = dev, "spun down disk"),
                Ok(s) => warn!(pool = pool.name, device = dev, code = ?s.code(), "hdparm -Y failed"),
                Err(e) => warn!(pool = pool.name, device = dev, error = %e, "hdparm -Y errored"),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = dev;
            warn!(pool = pool.name, "active spindown requested but not supported on this OS");
        }
    }
}

/// Tracks each pool's awake/asleep state across ticks so the manager can fire a
/// single spin-down on the awake→asleep transition.
#[derive(Default)]
pub struct StandbyManager {
    awake: HashMap<String, bool>,
}

static MANAGER: OnceLock<Mutex<StandbyManager>> = OnceLock::new();

fn manager() -> &'static Mutex<StandbyManager> {
    MANAGER.get_or_init(|| Mutex::new(StandbyManager::default()))
}

/// Drive window transitions once per scheduler tick. On a pool going
/// awake→asleep, spins its devices down (if `active_spindown` and nothing is
/// actively syncing it, per `busy`). `now` injected for testability in
/// [`StandbyManager::step`]; production passes `Local::now()`.
pub fn tick(pools: &[StandbyPoolConfig], busy: impl Fn(&str) -> bool) {
    let now = Local::now();
    let mut mgr = manager().lock().unwrap();
    mgr.step(pools, now, &busy);
}

impl StandbyManager {
    fn step(
        &mut self,
        pools: &[StandbyPoolConfig],
        now: DateTime<Local>,
        busy: &impl Fn(&str) -> bool,
    ) {
        for pool in pools.iter().filter(|p| p.enabled) {
            let open = is_within_wake_window(now, &pool.wake).unwrap_or(false);
            let was = self.awake.get(&pool.name).copied().unwrap_or(false);
            if open && !was {
                info!(pool = pool.name, "wake window opened");
                self.awake.insert(pool.name.clone(), true);
            } else if !open && was {
                // Don't park a disk mid-write; retry next tick once it's idle.
                if busy(&pool.name) {
                    continue;
                }
                info!(pool = pool.name, "wake window closed; parking disk");
                spin_down_devices(pool);
                self.awake.insert(pool.name.clone(), false);
            }
        }
    }

    /// Current awake state (for status display); unknown pools read as awake so
    /// a never-ticked pool never wrongly blocks (gate does the real check).
    pub fn is_awake(&self, name: &str) -> bool {
        self.awake.get(name).copied().unwrap_or(true)
    }
}

/// Snapshot the awake flag for a pool (status/UI).
pub fn pool_is_awake(name: &str) -> bool {
    manager().lock().unwrap().is_awake(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wake(every: u32, weekday: &str, time: &str, anchor: &str, win: u64) -> WakeSchedule {
        WakeSchedule {
            every_weeks: every,
            weekday: weekday.to_string(),
            time: time.to_string(),
            anchor_date: anchor.to_string(),
            max_window_minutes: win,
        }
    }

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, mi, 0).single().unwrap()
    }

    #[test]
    fn weekly_window_opens_on_weekday_and_lasts_the_window() {
        // 2026-01-03 is a Saturday (anchor). Weekly, opens 10:00 for 12h.
        let w = wake(1, "saturday", "10:00", "2026-01-03", 12 * 60);
        // Saturday 2026-01-10 (a week later) 11:00 → inside window.
        assert!(is_within_wake_window(at(2026, 1, 10, 11, 0), &w).unwrap());
        // 09:59 before open → asleep.
        assert!(!is_within_wake_window(at(2026, 1, 10, 9, 59), &w).unwrap());
        // 22:01 (past 10:00+12h) → asleep.
        assert!(!is_within_wake_window(at(2026, 1, 10, 22, 1), &w).unwrap());
        // Sunday → asleep.
        assert!(!is_within_wake_window(at(2026, 1, 11, 11, 0), &w).unwrap());
    }

    #[test]
    fn every_four_weeks_only_wakes_on_cadence_saturdays() {
        // anchor 2026-01-03 (Sat, week 0). every 4 weeks → weeks 0,4,8...
        let w = wake(4, "saturday", "10:00", "2026-01-03", 12 * 60);
        assert!(is_within_wake_window(at(2026, 1, 3, 11, 0), &w).unwrap()); // week 0
        assert!(!is_within_wake_window(at(2026, 1, 10, 11, 0), &w).unwrap()); // week 1
        assert!(!is_within_wake_window(at(2026, 1, 17, 11, 0), &w).unwrap()); // week 2
        assert!(!is_within_wake_window(at(2026, 1, 24, 11, 0), &w).unwrap()); // week 3
        assert!(is_within_wake_window(at(2026, 1, 31, 11, 0), &w).unwrap()); // week 4
    }

    #[test]
    fn next_wake_is_the_upcoming_cadence_day() {
        let w = wake(4, "saturday", "10:00", "2026-01-03", 12 * 60);
        // Thursday 2026-01-15 → next cadence Saturday is 2026-01-31 (week 4).
        let next = next_wake_start_after(at(2026, 1, 15, 9, 0), &w).unwrap();
        assert_eq!(next, at(2026, 1, 31, 10, 0));
    }

    #[test]
    fn gate_defers_when_pool_asleep_and_passes_when_no_pool_touched() {
        let pool = StandbyPoolConfig {
            name: "zfs".into(),
            mount_roots: vec![PathBuf::from("/zfs")],
            enabled: true,
            wake: wake(1, "saturday", "10:00", "2026-01-03", 12 * 60),
            ..Default::default()
        };
        let pools = vec![pool];
        // Monday → asleep; a task writing /zfs/wx is gated.
        let g = gate_for_roots(&pools, &[Path::new("/opt/wx"), Path::new("/zfs/wx")],
                               at(2026, 1, 5, 12, 0)).unwrap();
        match g {
            Some(Gate::Asleep { pool, .. }) => assert_eq!(pool, "zfs"),
            other => panic!("expected Asleep, got {other:?}"),
        }
        // A task entirely off the pool (/opt -> /opt) is never gated.
        assert!(gate_for_roots(&pools, &[Path::new("/opt/a"), Path::new("/opt/b")],
                               at(2026, 1, 5, 12, 0)).unwrap().is_none());
    }

    #[test]
    fn pool_covers_only_paths_under_its_roots() {
        let pool = StandbyPoolConfig {
            name: "zfs".into(),
            mount_roots: vec![PathBuf::from("/zfs")],
            ..Default::default()
        };
        assert!(pool_covers_path(&pool, Path::new("/zfs")));
        assert!(pool_covers_path(&pool, Path::new("/zfs/wx/a")));
        assert!(!pool_covers_path(&pool, Path::new("/zfs_pool/x")));
        assert!(!pool_covers_path(&pool, Path::new("/opt/wx")));
    }
}
