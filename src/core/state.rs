use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Serializes the heaviest/most-frequent writers across the in-process openers
/// (scheduler, watcher, web) so a long `replace_snapshot` transaction and the
/// watcher's per-event `record_event` queue on this lock instead of racing on
/// SQLite's single writer lock and surfacing "database is locked". All State
/// instances in the process share one DB file, so one lock covers them.
static DB_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Rows per transaction for bulk event_log deletes. Small enough that no
/// single transaction holds SQLite's writer long (keeping UI and watcher
/// writes responsive), large enough to amortize commit cost.
const EVENT_PRUNE_CHUNK: usize = 20_000;

use crate::core::config::AppConfig;
use crate::core::config::ScheduleMode;
use crate::core::scheduler;

#[derive(Debug, Clone)]
pub struct Cycle {
    pub id: i64,
    pub source_id: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub status: String,
    pub needs_full_rescan: bool,
    pub manual_full_rescan: bool,
    pub manual_changed_since_rescan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleEvent {
    pub event_id: i64,
    pub event_kind: String,
    pub rel_path: Option<String>,
    pub rescan_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub rel_path: String,
    pub file_type: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// Omitted from JSON when absent: whole-tree snapshots cross the wire with
    /// hundreds of thousands of entries, and a literal `"hash":null` per entry
    /// adds megabytes for nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// Lazy retention cap for the task log: inserts prune finished rows beyond
/// the newest this-many (running rows are exempt).
const TASK_LOG_KEEP: i64 = 100;

/// One recorded sync/compare task (running rows have no `ended_at` yet).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskLogEntry {
    pub id: i64,
    pub kind: String,
    pub source_id: String,
    pub destination_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_ms: Option<i64>,
    pub status: String,
    pub error: String,
    pub files_synced: u64,
    pub differences: u64,
    pub entries_scanned: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationView {
    pub source_id: String,
    pub destination_id: String,
    pub path: String,
    pub enabled: bool,
    pub latest_closed_cycle_id: Option<i64>,
    pub target_cycle_id: Option<i64>,
    pub last_verified_cycle_id: Option<i64>,
    pub last_completed_cycle_id: Option<i64>,
    pub status: String,
    pub status_reason: String,
    pub updated_at: Option<String>,
    pub issues: Vec<DestinationIssueView>,
    /// Differences the last successful Compare found (add+update+delete+
    /// type-mismatch); None when no report is stored or the compare failed.
    /// Drives the UI's repair affordance.
    #[serde(default)]
    pub scan_differences: Option<u64>,
    /// When the report behind `scan_differences` was taken.
    #[serde(default)]
    pub scan_at: Option<String>,
    /// Source-level restart notice (duplicated onto each of the source's
    /// destination views): the daemon restarted at this time on a machine
    /// whose watcher cannot see downtime changes. Persists until a covering
    /// manual action or a user dismissal.
    #[serde(default)]
    pub restart_notice_at: Option<String>,
    /// Start of the potentially unobserved window (the last event the
    /// previous run saw); None when unknown.
    #[serde(default)]
    pub restart_gap_started: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationIssueView {
    pub cycle_id: Option<i64>,
    pub rel_path: String,
    pub issue_kind: String,
    pub message: String,
    pub updated_at: String,
}

/// One differing path found by a dry-run Scan. `kind` is add | update | delete
/// | type_mismatch (relative to a mirror sync of source onto destination).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanDiffEntry {
    pub rel_path: String,
    pub kind: String,
    pub file_type: String,
}

/// Result of a dry-run Scan: how source and destination differ, without making
/// any change. `differences` is a capped sample; `truncated` flags more.
/// A non-empty `error` marks a failed scan attempt (the counts are then
/// meaningless); it is persisted so the UI can surface the failure instead of
/// waiting forever for a report that will never arrive.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScanReport {
    pub source_id: String,
    pub destination_id: String,
    pub scanned_at: String,
    pub source_entries: u64,
    pub dst_entries: u64,
    pub in_sync: u64,
    pub to_add: u64,
    pub to_update: u64,
    pub to_delete: u64,
    pub type_mismatch: u64,
    /// Content-equal files whose permission bits differ (repaired via chmod).
    #[serde(default)]
    pub metadata: u64,
    pub differences: Vec<ScanDiffEntry>,
    pub truncated: bool,
    #[serde(default)]
    pub error: String,
    /// How the compare was produced: "" for a full two-tree walk, "zfs_diff"
    /// when both sides were diffed against their verified base snapshots and
    /// only the changed paths were examined (entry counts then cover just
    /// those paths, not the whole tree).
    #[serde(default)]
    pub method: String,
}

/// One watcher event queued for [`State::record_events_batch`].
#[derive(Debug, Clone)]
pub struct WatcherEvent {
    pub source_id: String,
    pub raw_mask: u64,
    pub event_kind: String,
    pub rel_path: Option<String>,
    pub rescan_required: bool,
}

pub struct State {
    conn: Connection,
    /// Fingerprint of the last config applied via [`Self::ensure_config`], so
    /// a long-lived State (the scheduler polls every few seconds) does not
    /// re-upsert identical config rows into the database on every tick.
    applied_config_fingerprint: std::cell::Cell<Option<u64>>,
}

impl State {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create state dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open state db {}", path.display()))?;
        // Multiple openers (scheduler + web/UI) may touch this DB; wait rather
        // than fail immediately on a transient writer lock.
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let state = Self {
            conn,
            applied_config_fingerprint: std::cell::Cell::new(None),
        };
        state.init_schema()?;
        Ok(state)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS source_group (
                id TEXT PRIMARY KEY,
                src_path TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                mode TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS destination (
                source_id TEXT NOT NULL,
                destination_id TEXT NOT NULL,
                dst_path TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (source_id, destination_id)
            );

            CREATE TABLE IF NOT EXISTS sync_cycle (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                starts_at TEXT NOT NULL,
                ends_at TEXT,
                status TEXT NOT NULL,
                needs_full_rescan INTEGER NOT NULL DEFAULT 0,
                manual_full_rescan INTEGER NOT NULL DEFAULT 0,
                manual_changed_since_rescan INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_sync_cycle_source_status
            ON sync_cycle(source_id, status, id);

            CREATE TABLE IF NOT EXISTS event_log (
                event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                source_id TEXT NOT NULL,
                cycle_id INTEGER,
                observed_at TEXT NOT NULL,
                raw_mask INTEGER NOT NULL,
                event_kind TEXT NOT NULL,
                rel_path TEXT,
                rescan_required INTEGER NOT NULL DEFAULT 0,
                persisted_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_event_log_source_cycle
            ON event_log(source_id, cycle_id, event_id);

            CREATE TABLE IF NOT EXISTS destination_offset (
                source_id TEXT NOT NULL,
                destination_id TEXT NOT NULL,
                target_cycle_id INTEGER,
                last_completed_cycle_id INTEGER,
                last_verified_cycle_id INTEGER,
                status TEXT NOT NULL,
                status_reason TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (source_id, destination_id)
            );

            -- Historical whole-tree snapshot store: written by every full
            -- pass (hundreds of thousands of rows) but read by nothing since
            -- Changed-Since was removed. Dropped to reclaim old databases;
            -- harmless no-op on fresh ones.
            DROP TABLE IF EXISTS path_snapshot;

            CREATE TABLE IF NOT EXISTS destination_issue (
                source_id TEXT NOT NULL,
                destination_id TEXT NOT NULL,
                cycle_id INTEGER,
                rel_path TEXT NOT NULL,
                issue_kind TEXT NOT NULL,
                message TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (source_id, destination_id, rel_path, issue_kind)
            );

            CREATE TABLE IF NOT EXISTS windows_usn_cursor (
                source_id TEXT PRIMARY KEY,
                volume TEXT NOT NULL,
                journal_id TEXT NOT NULL,
                next_usn INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS scan_report (
                source_id TEXT NOT NULL,
                destination_id TEXT NOT NULL,
                scanned_at TEXT NOT NULL,
                report_json TEXT NOT NULL,
                PRIMARY KEY (source_id, destination_id)
            );

            CREATE TABLE IF NOT EXISTS task_log (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                destination_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                ended_at TEXT,
                duration_ms INTEGER,
                status TEXT NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                files_synced INTEGER NOT NULL DEFAULT 0,
                differences INTEGER NOT NULL DEFAULT 0,
                entries_scanned INTEGER NOT NULL DEFAULT 0
            );
            "#,
        )?;
        self.ensure_column("destination_offset", "target_cycle_id", "INTEGER")?;
        self.ensure_column("destination_offset", "last_completed_cycle_id", "INTEGER")?;
        self.ensure_column(
            "sync_cycle",
            "manual_full_rescan",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_column(
            "sync_cycle",
            "manual_changed_since_rescan",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        self.ensure_column("destination_offset", "last_verified_snapshot_name", "TEXT")?;
        self.ensure_column(
            "destination_offset",
            "last_verified_dst_snapshot_name",
            "TEXT",
        )?;
        // Per-source scalars that must survive event_log pruning.
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS source_meta (
                source_id TEXT PRIMARY KEY,
                last_event_observed_at TEXT
            );
            "#,
        )?;
        // Restart notice: raised when the daemon starts on a platform whose
        // watcher cannot observe downtime changes; persists until a covering
        // manual action (compare / full) or a user dismissal.
        self.ensure_column("source_meta", "restart_notice_at", "TEXT")?;
        self.ensure_column("source_meta", "restart_gap_started", "TEXT")?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, definition: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        for row in rows {
            if row? == column {
                return Ok(());
            }
        }
        self.conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
        Ok(())
    }

    pub fn ensure_config(&self, cfg: &AppConfig) -> Result<()> {
        // Skip the per-source/destination upserts when nothing changed: the
        // scheduler calls this every tick, and unconditional writes churn the
        // WAL and contend with UI writers even on a fully idle system.
        let fingerprint = config_fingerprint(cfg);
        if self.applied_config_fingerprint.get() == Some(fingerprint) {
            return Ok(());
        }
        // Web GET handlers open a FRESH State per request, so a per-instance
        // cell alone never hits: every /api/status poll re-upserted every row
        // (a real fsync each) on an idle system. The process-wide fingerprint
        // makes those requests read-only after the first application.
        {
            use std::sync::{Mutex, OnceLock};
            static APPLIED: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
            let applied = APPLIED.get_or_init(|| Mutex::new(None));
            let mut guard = applied.lock().unwrap_or_else(|err| err.into_inner());
            if *guard == Some(fingerprint) {
                self.applied_config_fingerprint.set(Some(fingerprint));
                return Ok(());
            }
            *guard = Some(fingerprint);
        }
        let now = now_string();
        for source in &cfg.source_groups {
            self.conn.execute(
                r#"
                INSERT INTO source_group (id, src_path, enabled, mode, created_at, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                ON CONFLICT(id) DO UPDATE SET
                    src_path=excluded.src_path,
                    enabled=excluded.enabled,
                    mode=excluded.mode,
                    updated_at=excluded.updated_at
                "#,
                params![
                    source.id,
                    source.src.to_string_lossy(),
                    bool_to_int(source.enabled),
                    "mirror",
                    now
                ],
            )?;

            for dst in &source.destinations {
                self.conn.execute(
                    r#"
                    INSERT INTO destination
                        (source_id, destination_id, dst_path, enabled, created_at, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                    ON CONFLICT(source_id, destination_id) DO UPDATE SET
                        dst_path=excluded.dst_path,
                        enabled=excluded.enabled,
                        updated_at=excluded.updated_at
                    "#,
                    params![
                        source.id,
                        dst.id,
                        dst.path.to_string_lossy(),
                        bool_to_int(dst.enabled),
                        now
                    ],
                )?;
                self.ensure_destination_offset(&source.id, &dst.id)?;
            }
        }
        self.applied_config_fingerprint.set(Some(fingerprint));
        Ok(())
    }

    fn ensure_destination_offset(&self, source_id: &str, destination_id: &str) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO destination_offset
                (source_id, destination_id, target_cycle_id, last_completed_cycle_id,
                 last_verified_cycle_id, status, status_reason, updated_at)
            VALUES (?1, ?2, NULL, NULL, NULL, 'red', 'not_verified', ?3)
            ON CONFLICT(source_id, destination_id) DO NOTHING
            "#,
            params![source_id, destination_id, now_string()],
        )?;
        Ok(())
    }

    pub fn ensure_open_cycles(&self, cfg: &AppConfig) -> Result<()> {
        for source in cfg.source_groups.iter().filter(|s| s.enabled) {
            self.ensure_open_cycle(&source.id, Utc::now())?;
        }
        Ok(())
    }

    pub fn ensure_open_cycle(&self, source_id: &str, starts_at: DateTime<Utc>) -> Result<i64> {
        // check-then-insert must be atomic: scheduler, watcher and web
        // requests each hold their own connection, and two concurrent misses
        // used to insert two 'open' rows (the older one lingered forever).
        // INSERT ... WHERE NOT EXISTS makes the recheck and the insert one
        // statement; a racing insert between it and the reread just wins.
        if let Some(id) = self.current_open_cycle_id(source_id)? {
            return Ok(id);
        }
        let now = starts_at.to_rfc3339();
        self.conn.execute(
            r#"
            INSERT INTO sync_cycle
                (source_id, starts_at, status, needs_full_rescan, created_at, updated_at)
            SELECT ?1, ?2, 'open', 0, ?2, ?2
            WHERE NOT EXISTS (
                SELECT 1 FROM sync_cycle WHERE source_id = ?1 AND status = 'open'
            )
            "#,
            params![source_id, now],
        )?;
        self.current_open_cycle_id(source_id)?
            .ok_or_else(|| anyhow::anyhow!("failed to create open cycle for {source_id}"))
    }

    pub fn current_open_cycle_id(&self, source_id: &str) -> Result<Option<i64>> {
        self.conn
            .query_row(
                "SELECT id FROM sync_cycle WHERE source_id=?1 AND status='open' ORDER BY id DESC LIMIT 1",
                params![source_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn close_due_cycles(&self, cfg: &AppConfig, force: bool) -> Result<Vec<Cycle>> {
        self.ensure_config(cfg)?;
        self.ensure_open_cycles(cfg)?;
        let now = Utc::now();
        let mut closed = Vec::new();
        for source in cfg.source_groups.iter().filter(|s| s.enabled) {
            let Some(cycle) = self.current_open_cycle(&source.id)? else {
                continue;
            };
            if !force {
                continue;
            }
            self.conn.execute(
                r#"
                UPDATE sync_cycle
                SET status='closed', ends_at=?1, updated_at=?1
                WHERE id=?2 AND status='open'
                "#,
                params![now.to_rfc3339(), cycle.id],
            )?;
            self.ensure_open_cycle(&source.id, now)?;
            let mut closed_cycle = cycle;
            closed_cycle.status = "closed".to_string();
            closed_cycle.ends_at = Some(now);
            closed.push(closed_cycle);
        }
        Ok(closed)
    }

    pub fn advance_due_destination_targets(&self, cfg: &AppConfig) -> Result<Vec<Cycle>> {
        self.ensure_config(cfg)?;
        self.ensure_open_cycles(cfg)?;
        let now = Utc::now();
        let mut closed = Vec::new();

        for source in cfg.source_groups.iter().filter(|s| s.enabled) {
            let Some(cycle) = self.current_open_cycle(&source.id)? else {
                continue;
            };
            let open_has_events = self.cycle_has_actionable_events(cycle.id)?;
            let mut due_destinations = Vec::new();

            for dst in source.destinations.iter().filter(|d| d.enabled && !d.paused) {
                let offset = self.destination_offset(&source.id, &dst.id)?;
                if let Some(target) = offset.target_cycle_id {
                    if offset.last_verified_cycle_id < Some(target) {
                        continue;
                    }
                }

                let first_sync =
                    offset.target_cycle_id.is_none() && offset.last_verified_cycle_id.is_none();
                let due = if first_sync {
                    true
                } else if dst.schedule.mode == ScheduleMode::Realtime {
                    open_has_events
                } else {
                    scheduler::cycle_is_due(cycle.starts_at, now, &dst.schedule)
                };
                if due {
                    due_destinations.push(dst.id.clone());
                }
            }

            if due_destinations.is_empty() {
                // No destination is due, but accumulated events should still
                // advance the source's cycle so the UI shows work piling up
                // (e.g. "verified 6 / latest 9" on a Saturday-scheduled
                // destination). Close WITHOUT setting any target: the actual
                // transfer waits for the schedule, which then applies the
                // whole event backlog across the skipped cycles. Debounced on
                // event quiescence so a steady write burst does not mint a
                // cycle every scheduler tick.
                if open_has_events && self.source_events_quiesced(&source.id)? {
                    if let Some(closed_cycle) = self.close_current_cycle_for_source(&source.id)? {
                        closed.push(closed_cycle);
                    }
                }
                continue;
            }

            let Some(closed_cycle) = self.close_current_cycle_for_source(&source.id)? else {
                continue;
            };
            if !closed_cycle.needs_full_rescan
                && due_destinations.iter().all(|destination_id| {
                    source.destinations.iter().any(|dst| {
                        dst.id == *destination_id && dst.schedule.mode == ScheduleMode::Realtime
                    })
                })
            {
                self.clear_cycle_needs_rescan(closed_cycle.id)?;
            }
            for destination_id in &due_destinations {
                self.set_destination_target(&source.id, destination_id, closed_cycle.id)?;
            }
            closed.push(closed_cycle);
        }

        Ok(closed)
    }

    pub fn force_target_all_destinations(&self, cfg: &AppConfig) -> Result<Vec<Cycle>> {
        self.ensure_config(cfg)?;
        self.ensure_open_cycles(cfg)?;
        let mut closed = Vec::new();
        for source in cfg.source_groups.iter().filter(|s| s.enabled) {
            let Some(cycle) = self.close_current_cycle_for_source(&source.id)? else {
                continue;
            };
            // Paused destinations are excluded: Sync All must not queue work
            // that would spring back to life on resume unasked.
            for dst in source.destinations.iter().filter(|d| d.enabled && !d.paused) {
                self.set_destination_target(&source.id, &dst.id, cycle.id)?;
            }
            closed.push(cycle);
        }
        Ok(closed)
    }

    pub fn force_target_source(&self, cfg: &AppConfig, source_id: &str) -> Result<Option<Cycle>> {
        self.ensure_config(cfg)?;
        self.ensure_open_cycles(cfg)?;
        let Some(source) = cfg
            .source_groups
            .iter()
            .find(|source| source.id == source_id && source.enabled)
        else {
            return Ok(None);
        };
        let Some(cycle) = self.close_current_cycle_for_source(source_id)? else {
            return Ok(None);
        };
        for dst in source.destinations.iter().filter(|d| d.enabled && !d.paused) {
            self.set_destination_target(source_id, &dst.id, cycle.id)?;
        }
        Ok(Some(cycle))
    }

    pub fn force_target_destination(
        &self,
        cfg: &AppConfig,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<Cycle>> {
        self.ensure_config(cfg)?;
        self.ensure_open_cycles(cfg)?;
        let Some(source) = cfg
            .source_groups
            .iter()
            .find(|source| source.id == source_id && source.enabled)
        else {
            return Ok(None);
        };
        if !source
            .destinations
            .iter()
            .any(|dst| dst.id == destination_id && dst.enabled)
        {
            return Ok(None);
        }
        let Some(cycle) = self.close_current_cycle_for_source(source_id)? else {
            return Ok(None);
        };
        self.set_destination_target(source_id, destination_id, cycle.id)?;
        Ok(Some(cycle))
    }

    pub fn close_current_cycle_for_source(&self, source_id: &str) -> Result<Option<Cycle>> {
        let now = Utc::now();
        let Some(cycle) = self.current_open_cycle(source_id)? else {
            return Ok(None);
        };
        self.conn.execute(
            r#"
            UPDATE sync_cycle
            SET status='closed', ends_at=?1, updated_at=?1
            WHERE id=?2 AND status='open'
            "#,
            params![now.to_rfc3339(), cycle.id],
        )?;
        self.ensure_open_cycle(source_id, now)?;
        let mut closed_cycle = cycle;
        closed_cycle.status = "closed".to_string();
        closed_cycle.ends_at = Some(now);
        crate::core::peer_notify::mark_local_change(source_id);
        Ok(Some(closed_cycle))
    }

    pub fn current_open_cycle(&self, source_id: &str) -> Result<Option<Cycle>> {
        self.conn
            .query_row(
                r#"
                SELECT id, source_id, starts_at, ends_at, status, needs_full_rescan,
                       manual_full_rescan, manual_changed_since_rescan
                FROM sync_cycle
                WHERE source_id=?1 AND status='open'
                ORDER BY id DESC
                LIMIT 1
                "#,
                params![source_id],
                cycle_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn closed_cycles_for_source(&self, source_id: &str) -> Result<Vec<Cycle>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, source_id, starts_at, ends_at, status, needs_full_rescan,
                   manual_full_rescan, manual_changed_since_rescan
            FROM sync_cycle
            WHERE source_id=?1 AND status IN ('closed', 'planning', 'syncing', 'failed')
            ORDER BY id ASC
            "#,
        )?;
        let rows = stmt.query_map(params![source_id], cycle_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn cycle_has_events(&self, cycle_id: i64) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM event_log WHERE cycle_id=?1",
            params![cycle_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn cycle_has_actionable_events(&self, cycle_id: i64) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM event_log
            WHERE cycle_id=?1 AND (rel_path IS NOT NULL OR rescan_required <> 0)
            "#,
            params![cycle_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Events recorded in cycles AFTER `after_cycle_id` and up to (including)
    /// `through_cycle_id` — the accumulated backlog a destination must apply
    /// to advance from its last verified cycle to the target cycle. Scheduled
    /// destinations skip many intermediate cycles, so a single cycle's events
    /// are not enough for them.
    pub fn events_between_cycles(
        &self,
        source_id: &str,
        after_cycle_id: i64,
        through_cycle_id: i64,
    ) -> Result<Vec<CycleEvent>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT event_id, event_kind, rel_path, rescan_required
            FROM event_log
            WHERE source_id=?1 AND cycle_id>?2 AND cycle_id<=?3
            ORDER BY event_id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![source_id, after_cycle_id, through_cycle_id],
            |row| {
                Ok(CycleEvent {
                    event_id: row.get(0)?,
                    event_kind: row.get(1)?,
                    rel_path: row.get(2)?,
                    rescan_required: row.get::<_, i64>(3)? != 0,
                })
            },
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn cycle_events(&self, source_id: &str, cycle_id: i64) -> Result<Vec<CycleEvent>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT event_id, event_kind, rel_path, rescan_required
            FROM event_log
            WHERE source_id=?1 AND cycle_id=?2
            ORDER BY event_id ASC
            "#,
        )?;
        let rows = stmt.query_map(params![source_id, cycle_id], |row| {
            Ok(CycleEvent {
                event_id: row.get(0)?,
                event_kind: row.get(1)?,
                rel_path: row.get(2)?,
                rescan_required: row.get::<_, i64>(3)? != 0,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// True when the source's newest event is old enough that the current
    /// write burst has settled (or no event time is known). Used to debounce
    /// closing target-less cycles for scheduled-only destinations.
    fn source_events_quiesced(&self, source_id: &str) -> Result<bool> {
        let quiescence = chrono::Duration::seconds(10);
        Ok(self
            .latest_event_observed_at(source_id)?
            .is_none_or(|at| Utc::now() - at >= quiescence))
    }

    pub fn latest_event_observed_at(&self, source_id: &str) -> Result<Option<DateTime<Utc>>> {
        // Prefer the pruning-proof scalar; fall back to scanning event_log for
        // databases written before source_meta existed.
        let meta: Option<String> = self
            .conn
            .query_row(
                "SELECT last_event_observed_at FROM source_meta WHERE source_id=?1",
                params![source_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        if let Some(value) = meta {
            return Ok(Some(parse_db_time(&value)?));
        }
        let value: Option<String> = self
            .conn
            .query_row(
                r#"
                SELECT observed_at
                FROM event_log
                WHERE source_id=?1
                ORDER BY observed_at DESC, event_id DESC
                LIMIT 1
                "#,
                params![source_id],
                |row| row.get(0),
            )
            .optional()?;
        value.as_deref().map(parse_db_time).transpose()
    }

    pub fn event_paths_observed_since(
        &self,
        source_id: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT DISTINCT rel_path
            FROM event_log
            WHERE source_id=?1 AND observed_at>=?2 AND rel_path IS NOT NULL
            "#,
        )?;
        let rows = stmt.query_map(params![source_id, observed_at.to_rfc3339()], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<rusqlite::Result<std::collections::HashSet<_>>>()
            .map_err(Into::into)
    }

    pub fn source_has_target_cycle(&self, source_id: &str, cycle_id: i64) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            r#"
            SELECT COUNT(*)
            FROM destination_offset
            WHERE source_id=?1 AND target_cycle_id=?2
            "#,
            params![source_id, cycle_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn latest_closed_cycle_id(&self, source_id: &str) -> Result<Option<i64>> {
        let value = self.conn.query_row(
            r#"
            SELECT MAX(id)
            FROM sync_cycle
            WHERE source_id=?1 AND status <> 'open'
            "#,
            params![source_id],
            |row| row.get(0),
        )?;
        Ok(value)
    }

    pub fn cycle_by_id(&self, source_id: &str, cycle_id: i64) -> Result<Option<Cycle>> {
        self.conn
            .query_row(
                r#"
                SELECT id, source_id, starts_at, ends_at, status, needs_full_rescan,
                       manual_full_rescan, manual_changed_since_rescan
                FROM sync_cycle
                WHERE source_id=?1 AND id=?2
                "#,
                params![source_id, cycle_id],
                cycle_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn mark_cycle_status(&self, cycle_id: i64, status: &str) -> Result<()> {
        // NOTE: verified cycles must NOT eagerly delete their events (the old
        // startup_mtime_scan special case): a scheduled destination
        // accumulates events across many cycles and applies them at its
        // schedule time, so events live until every enabled destination has
        // verified past their cycle (see `prune_event_log`).
        self.conn.execute(
            "UPDATE sync_cycle SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn mark_cycle_needs_rescan(&self, cycle_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_cycle SET needs_full_rescan=1, updated_at=?1 WHERE id=?2",
            params![now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn mark_cycle_manual_full_rescan(&self, cycle_id: i64) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE sync_cycle
            SET needs_full_rescan=1, manual_full_rescan=1, updated_at=?1
            WHERE id=?2
            "#,
            params![now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn clear_cycle_needs_rescan(&self, cycle_id: i64) -> Result<()> {
        // Only the event-loss flag — and only when nothing manual arrived in
        // the meantime. This used to zero manual_full_rescan too, based on a
        // snapshot read before the cycle closed: a manual Full requested in
        // that window was silently discarded.
        self.conn.execute(
            r#"
            UPDATE sync_cycle
            SET needs_full_rescan=0,
                updated_at=?1
            WHERE id=?2 AND manual_full_rescan=0
            "#,
            params![now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn mark_open_cycle_needs_rescan(&self, source_id: &str) -> Result<()> {
        let cycle_id = self.ensure_open_cycle(source_id, Utc::now())?;
        self.mark_cycle_needs_rescan(cycle_id)
    }

    pub fn record_event(
        &self,
        source_id: &str,
        raw_mask: u64,
        event_kind: &str,
        rel_path: Option<&str>,
        rescan_required: bool,
    ) -> Result<i64> {
        let _write = DB_WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let cycle_id = self.ensure_open_cycle(source_id, Utc::now())?;
        let now = now_string();
        self.conn.execute(
            r#"
            INSERT INTO event_log
                (source_id, cycle_id, observed_at, raw_mask, event_kind, rel_path,
                 rescan_required, persisted_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?3)
            "#,
            params![
                source_id,
                cycle_id,
                now,
                raw_mask as i64,
                event_kind,
                rel_path,
                bool_to_int(rescan_required)
            ],
        )?;
        let event_id = self.conn.last_insert_rowid();
        // Keep the per-source "latest event" scalar; the startup gap scan must
        // survive event_log pruning.
        self.conn.execute(
            r#"
            INSERT INTO source_meta (source_id, last_event_observed_at)
            VALUES (?1, ?2)
            ON CONFLICT(source_id) DO UPDATE SET
                last_event_observed_at=MAX(COALESCE(last_event_observed_at, ''), excluded.last_event_observed_at)
            "#,
            params![source_id, now],
        )?;
        if rescan_required {
            self.mark_open_cycle_needs_rescan(source_id)?;
        }
        // Wake the scheduler so realtime changes sync immediately instead of
        // waiting out its polling interval.
        crate::core::signal::notify_scheduler();
        Ok(event_id)
    }

    /// Record a whole watcher read() batch in one transaction (one WAL fsync)
    /// instead of 2-3 autocommits per event. With `synchronous=FULL` the
    /// per-event fsyncs capped persistence at a few dozen events per second on
    /// spinning disks — and a slow watcher is exactly what overflows the
    /// kernel queue, whose penalty is a full-tree reconcile.
    pub fn record_events_batch(&self, events: &[WatcherEvent]) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }
        let _write = DB_WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let now = now_string();
        let tx = self.conn.unchecked_transaction()?;
        let mut rescan_sources: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut cycles: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
        for event in events {
            let cycle_id = match cycles.get(event.source_id.as_str()) {
                Some(id) => *id,
                None => {
                    let id = self.ensure_open_cycle(&event.source_id, Utc::now())?;
                    cycles.insert(event.source_id.as_str(), id);
                    id
                }
            };
            self.conn.execute(
                r#"
                INSERT INTO event_log
                    (source_id, cycle_id, observed_at, raw_mask, event_kind, rel_path,
                     rescan_required, persisted_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?3)
                "#,
                params![
                    event.source_id,
                    cycle_id,
                    now,
                    event.raw_mask as i64,
                    event.event_kind,
                    event.rel_path,
                    bool_to_int(event.rescan_required)
                ],
            )?;
            if event.rescan_required {
                rescan_sources.insert(event.source_id.as_str());
            }
        }
        for source_id in cycles.keys() {
            self.conn.execute(
                r#"
                INSERT INTO source_meta (source_id, last_event_observed_at)
                VALUES (?1, ?2)
                ON CONFLICT(source_id) DO UPDATE SET
                    last_event_observed_at=MAX(COALESCE(last_event_observed_at, ''), excluded.last_event_observed_at)
                "#,
                params![source_id, now],
            )?;
        }
        for source_id in &rescan_sources {
            self.mark_open_cycle_needs_rescan(source_id)?;
        }
        tx.commit()?;
        crate::core::signal::notify_scheduler();
        Ok(())
    }

    /// Delete destination_offset rows for (source, destination) pairs that no
    /// longer exist in the config at all (disabled pairs keep their row so
    /// re-enabling resumes where it left off). Returns the removed rows'
    /// recorded dst baseline snapshot names for best-effort destruction: a
    /// deleted row otherwise pinned its source snapshot forever through
    /// `source_referenced_snapshots`, and its dstbase snapshot leaked on disk.
    pub fn prune_removed_destination_offsets(&self, cfg: &AppConfig) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, destination_id, last_verified_dst_snapshot_name
             FROM destination_offset",
        )?;
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let mut orphan_snapshots = Vec::new();
        let mut removed = Vec::new();
        for (source_id, destination_id, dst_snapshot) in rows {
            let known = cfg.source_groups.iter().any(|source| {
                source.id == source_id
                    && source
                        .destinations
                        .iter()
                        .any(|dst| dst.id == destination_id)
            });
            if known {
                continue;
            }
            removed.push((source_id, destination_id));
            if let Some(name) = dst_snapshot {
                orphan_snapshots.push(name);
            }
        }
        if !removed.is_empty() {
            let _write = DB_WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
            for (source_id, destination_id) in &removed {
                self.conn.execute(
                    "DELETE FROM destination_offset WHERE source_id = ?1 AND destination_id = ?2",
                    params![source_id, destination_id],
                )?;
            }
        }
        Ok(orphan_snapshots)
    }

    pub fn upsert_destination_status(
        &self,
        source_id: &str,
        destination_id: &str,
        last_verified_cycle_id: Option<i64>,
        status: &str,
        reason: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO destination_offset
                (source_id, destination_id, target_cycle_id, last_completed_cycle_id,
                 last_verified_cycle_id, status, status_reason, updated_at)
            VALUES (?1, ?2, NULL, ?3, ?3, ?4, ?5, ?6)
            ON CONFLICT(source_id, destination_id) DO UPDATE SET
                last_completed_cycle_id=COALESCE(excluded.last_completed_cycle_id, last_completed_cycle_id),
                last_verified_cycle_id=COALESCE(excluded.last_verified_cycle_id, last_verified_cycle_id),
                status=excluded.status,
                status_reason=excluded.status_reason,
                updated_at=excluded.updated_at
            "#,
            params![
                source_id,
                destination_id,
                last_verified_cycle_id,
                status,
                reason,
                now_string()
            ],
        )?;
        crate::core::peer_notify::mark_local_change(source_id);
        Ok(())
    }

    /// The ZFS snapshot a destination was last verified against, used as the
    /// base for the next `zfs diff` incremental. `None` when the destination has
    /// never completed a snapshot-backed sync.
    pub fn destination_verified_snapshot(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT last_verified_snapshot_name FROM destination_offset WHERE source_id=?1 AND destination_id=?2",
                params![source_id, destination_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|opt| opt.flatten())
            .map_err(Into::into)
    }

    pub fn set_destination_verified_snapshot(
        &self,
        source_id: &str,
        destination_id: &str,
        snapshot_name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE destination_offset SET last_verified_snapshot_name=?3, updated_at=?4 WHERE source_id=?1 AND destination_id=?2",
            params![source_id, destination_id, snapshot_name, now_string()],
        )?;
        Ok(())
    }

    /// The DESTINATION-side ZFS snapshot taken at the destination's last
    /// verify (its content then equalled the source base). Together with
    /// [`Self::destination_verified_snapshot`] it lets Compare diff each side
    /// against its base instead of walking both trees.
    pub fn destination_verified_dst_snapshot(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT last_verified_dst_snapshot_name FROM destination_offset WHERE source_id=?1 AND destination_id=?2",
                params![source_id, destination_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|opt| opt.flatten())
            .map_err(Into::into)
    }

    pub fn set_destination_verified_dst_snapshot(
        &self,
        source_id: &str,
        destination_id: &str,
        snapshot_name: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE destination_offset SET last_verified_dst_snapshot_name=?3, updated_at=?4 WHERE source_id=?1 AND destination_id=?2",
            params![source_id, destination_id, snapshot_name, now_string()],
        )?;
        Ok(())
    }

    /// Snapshot names still referenced as a diff base by any destination of this
    /// source. The snapshot cleaner must retain these.
    pub fn source_referenced_snapshots(&self, source_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT last_verified_snapshot_name FROM destination_offset \
             WHERE source_id=?1 AND last_verified_snapshot_name IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![source_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn replace_destination_issues(
        &self,
        source_id: &str,
        destination_id: &str,
        cycle_id: i64,
        issue_kind: &str,
        paths: &[String],
        message: &str,
    ) -> Result<()> {
        self.conn.execute(
            "DELETE FROM destination_issue WHERE source_id=?1 AND destination_id=?2 AND issue_kind=?3",
            params![source_id, destination_id, issue_kind],
        )?;
        let now = now_string();
        for path in paths {
            self.conn.execute(
                r#"
                INSERT INTO destination_issue
                    (source_id, destination_id, cycle_id, rel_path, issue_kind, message, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(source_id, destination_id, rel_path, issue_kind) DO UPDATE SET
                    cycle_id=excluded.cycle_id,
                    message=excluded.message,
                    updated_at=excluded.updated_at
                "#,
                params![
                    source_id,
                    destination_id,
                    cycle_id,
                    path,
                    issue_kind,
                    message,
                    now
                ],
            )?;
        }
        Ok(())
    }

    pub fn clear_destination_issues(&self, source_id: &str, destination_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM destination_issue WHERE source_id=?1 AND destination_id=?2",
            params![source_id, destination_id],
        )?;
        Ok(())
    }

    pub fn put_scan_report(&self, report: &ScanReport) -> Result<()> {
        let json = serde_json::to_string(report).context("failed to encode scan report")?;
        self.conn.execute(
            r#"
            INSERT INTO scan_report (source_id, destination_id, scanned_at, report_json)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(source_id, destination_id) DO UPDATE SET
                scanned_at=excluded.scanned_at,
                report_json=excluded.report_json
            "#,
            params![
                report.source_id,
                report.destination_id,
                report.scanned_at,
                json
            ],
        )?;
        Ok(())
    }

    pub fn delete_scan_report(&self, source_id: &str, destination_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM scan_report WHERE source_id=?1 AND destination_id=?2",
            params![source_id, destination_id],
        )?;
        Ok(())
    }

    /// Raise the restart notice for a source: the daemon just started and its
    /// watcher cannot know what changed while it was down. Keeps an existing
    /// (undismissed) notice untouched so the reported gap never shrinks; the
    /// gap start defaults to the last event the previous run observed.
    pub fn raise_restart_notice(&self, source_id: &str) -> Result<bool> {
        self.conn.execute(
            "INSERT OR IGNORE INTO source_meta (source_id) VALUES (?1)",
            params![source_id],
        )?;
        let changed = self.conn.execute(
            "UPDATE source_meta
             SET restart_notice_at = ?2,
                 restart_gap_started = last_event_observed_at
             WHERE source_id = ?1 AND restart_notice_at IS NULL",
            params![source_id, Utc::now().to_rfc3339()],
        )?;
        Ok(changed > 0)
    }

    /// Clear the restart notice ONLY when a covering action began after the
    /// notice was raised — an action already underway during the restart read
    /// the tree too early to vouch for the gap.
    pub fn clear_restart_notice_if_covered(
        &self,
        source_id: &str,
        action_started_at: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE source_meta
             SET restart_notice_at = NULL, restart_gap_started = NULL
             WHERE source_id = ?1
               AND restart_notice_at IS NOT NULL
               AND restart_notice_at <= ?2",
            params![source_id, action_started_at],
        )?;
        Ok(changed > 0)
    }

    /// Unconditional clear: the user chose to ignore the notice.
    pub fn dismiss_restart_notice(&self, source_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE source_meta
             SET restart_notice_at = NULL, restart_gap_started = NULL
             WHERE source_id = ?1",
            params![source_id],
        )?;
        Ok(())
    }

    /// (noticed_at, gap_started) when the source has an active restart notice.
    pub fn restart_notice(&self, source_id: &str) -> Result<Option<(String, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT restart_notice_at, restart_gap_started
             FROM source_meta WHERE source_id = ?1 AND restart_notice_at IS NOT NULL",
        )?;
        let mut rows = stmt.query(params![source_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
            None => Ok(None),
        }
    }

    /// Open a task-log row for a starting sync/compare (`status = running`,
    /// no end time yet — a running task is queryable at any moment). Lazy
    /// retention: inserting prunes finished rows beyond the newest
    /// [`TASK_LOG_KEEP`]; running rows are never pruned.
    pub fn task_start(&self, kind: &str, source_id: &str, destination_id: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO task_log (kind, source_id, destination_id, started_at, status)
             VALUES (?1, ?2, ?3, ?4, 'running')",
            params![kind, source_id, destination_id, Utc::now().to_rfc3339()],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "DELETE FROM task_log
             WHERE ended_at IS NOT NULL
               AND id NOT IN (SELECT id FROM task_log ORDER BY id DESC LIMIT ?1)",
            params![TASK_LOG_KEEP],
        )?;
        Ok(id)
    }

    /// Close a task-log row with its outcome and counters.
    pub fn task_finish(
        &self,
        task_id: i64,
        status: &str,
        error: &str,
        files_synced: u64,
        differences: u64,
        entries_scanned: u64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE task_log
             SET ended_at = ?2,
                 duration_ms = CAST((julianday(?2) - julianday(started_at)) * 86400000 AS INTEGER),
                 status = ?3,
                 error = ?4,
                 files_synced = ?5,
                 differences = ?6,
                 entries_scanned = ?7
             WHERE id = ?1",
            params![
                task_id,
                Utc::now().to_rfc3339(),
                status,
                error,
                files_synced as i64,
                differences as i64,
                entries_scanned as i64
            ],
        )?;
        Ok(())
    }

    /// Drop a task row that turned out to be a no-op (nothing targeted or
    /// transferred) so the log holds real work, not scheduler heartbeats.
    pub fn task_discard(&self, task_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM task_log WHERE id = ?1", params![task_id])?;
        Ok(())
    }

    /// A daemon restart orphans `running` rows (the work died with the
    /// process); mark them so they don't read as live forever.
    pub fn abort_stale_running_tasks(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let changed = self.conn.execute(
            "UPDATE task_log
             SET ended_at = ?1,
                 duration_ms = CAST((julianday(?1) - julianday(started_at)) * 86400000 AS INTEGER),
                 status = 'aborted',
                 error = 'daemon restarted while the task was running'
             WHERE status = 'running'",
            params![now],
        )?;
        Ok(changed)
    }

    /// A single task row by id.
    pub fn task_by_id(&self, task_id: i64) -> Result<Option<TaskLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, source_id, destination_id, started_at, ended_at, duration_ms,
                    status, error, files_synced, differences, entries_scanned
             FROM task_log WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![task_id], |row| {
            Ok(TaskLogEntry {
                id: row.get(0)?,
                kind: row.get(1)?,
                source_id: row.get(2)?,
                destination_id: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                duration_ms: row.get(6)?,
                status: row.get(7)?,
                error: row.get(8)?,
                files_synced: row.get::<_, i64>(9)? as u64,
                differences: row.get::<_, i64>(10)? as u64,
                entries_scanned: row.get::<_, i64>(11)? as u64,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Newest-first task rows (running first by recency like the rest).
    pub fn recent_tasks(&self, limit: usize) -> Result<Vec<TaskLogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, source_id, destination_id, started_at, ended_at, duration_ms,
                    status, error, files_synced, differences, entries_scanned
             FROM task_log ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(TaskLogEntry {
                id: row.get(0)?,
                kind: row.get(1)?,
                source_id: row.get(2)?,
                destination_id: row.get(3)?,
                started_at: row.get(4)?,
                ended_at: row.get(5)?,
                duration_ms: row.get(6)?,
                status: row.get(7)?,
                error: row.get(8)?,
                files_synced: row.get::<_, i64>(9)? as u64,
                differences: row.get::<_, i64>(10)? as u64,
                entries_scanned: row.get::<_, i64>(11)? as u64,
            })
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    pub fn get_scan_report(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<ScanReport>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT report_json FROM scan_report WHERE source_id=?1 AND destination_id=?2",
                params![source_id, destination_id],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(json) => Ok(Some(
                serde_json::from_str(&json).context("failed to decode scan report")?,
            )),
            None => Ok(None),
        }
    }

    pub fn set_destination_target(
        &self,
        source_id: &str,
        destination_id: &str,
        target_cycle_id: i64,
    ) -> Result<()> {
        self.clear_destination_issues(source_id, destination_id)?;
        self.conn.execute(
            r#"
            INSERT INTO destination_offset
                (source_id, destination_id, target_cycle_id, last_completed_cycle_id,
                 last_verified_cycle_id, status, status_reason, updated_at)
            VALUES (?1, ?2, ?3, NULL, NULL, 'red', 'pending_target_cycle', ?4)
            ON CONFLICT(source_id, destination_id) DO UPDATE SET
                target_cycle_id=excluded.target_cycle_id,
                status='red',
                status_reason='pending_target_cycle',
                updated_at=excluded.updated_at
            "#,
            params![source_id, destination_id, target_cycle_id, now_string()],
        )?;
        crate::core::peer_notify::mark_local_change(source_id);
        Ok(())
    }

    /// Drop only the in-flight target (keeping the verified baseline intact),
    /// so a cancelled destination stops being re-driven by the scheduler until
    /// its schedule, a new event, or a manual sync targets it again.
    pub fn clear_destination_target(
        &self,
        source_id: &str,
        destination_id: &str,
        reason: &str,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE destination_offset
            SET target_cycle_id=NULL,
                status='red',
                status_reason=?3,
                updated_at=?4
            WHERE source_id=?1 AND destination_id=?2
            "#,
            params![source_id, destination_id, reason, now_string()],
        )?;
        crate::core::peer_notify::mark_local_change(source_id);
        Ok(())
    }

    pub fn reset_destination_offset(
        &self,
        source_id: &str,
        destination_id: &str,
        reason: &str,
    ) -> Result<()> {
        self.clear_destination_issues(source_id, destination_id)?;
        self.conn.execute(
            r#"
            INSERT INTO destination_offset
                (source_id, destination_id, target_cycle_id, last_completed_cycle_id,
                 last_verified_cycle_id, status, status_reason, updated_at)
            VALUES (?1, ?2, NULL, NULL, NULL, 'red', ?3, ?4)
            ON CONFLICT(source_id, destination_id) DO UPDATE SET
                target_cycle_id=NULL,
                last_completed_cycle_id=NULL,
                last_verified_cycle_id=NULL,
                last_verified_snapshot_name=NULL,
                last_verified_dst_snapshot_name=NULL,
                status='red',
                status_reason=excluded.status_reason,
                updated_at=excluded.updated_at
            "#,
            params![source_id, destination_id, reason, now_string()],
        )?;
        Ok(())
    }

    pub fn destination_target_cycle(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<i64>> {
        Ok(self
            .destination_offset(source_id, destination_id)?
            .target_cycle_id)
    }

    pub fn destination_last_verified(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Option<i64>> {
        let value: Option<Option<i64>> = self
            .conn
            .query_row(
                r#"
                SELECT last_verified_cycle_id
                FROM destination_offset
                WHERE source_id=?1 AND destination_id=?2
                "#,
                params![source_id, destination_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.flatten())
    }

    pub fn destination_views(&self, cfg: &AppConfig) -> Result<Vec<DestinationView>> {
        let mut views = Vec::new();
        for source in &cfg.source_groups {
            let latest = self.latest_closed_cycle_id(&source.id)?;
            let (restart_notice_at, restart_gap_started) = match self.restart_notice(&source.id)? {
                Some((at, gap)) => (Some(at), gap),
                None => (None, None),
            };
            for dst in &source.destinations {
                let offset = self.destination_offset(&source.id, &dst.id)?;
                let target = offset.target_cycle_id;
                let computed_status = if offset.status == "green"
                    && target.is_some()
                    && offset.last_verified_cycle_id >= target
                {
                    "green".to_string()
                } else if offset.status == "yellow" {
                    "yellow".to_string()
                } else {
                    "red".to_string()
                };
                let computed_reason = if target.is_some()
                    && offset.last_verified_cycle_id < target
                    && matches!(
                        offset.status_reason.as_str(),
                        "not_verified" | "pending_target_cycle" | "verified"
                    ) {
                    "behind_target_cycle".to_string()
                } else {
                    offset.status_reason
                };
                let (scan_differences, scan_at) = match self.get_scan_report(&source.id, &dst.id) {
                    Ok(Some(report)) if report.error.is_empty() => (
                        Some(
                            report.to_add
                                + report.to_update
                                + report.to_delete
                                + report.type_mismatch,
                        ),
                        Some(report.scanned_at),
                    ),
                    _ => (None, None),
                };
                views.push(DestinationView {
                    source_id: source.id.clone(),
                    destination_id: dst.id.clone(),
                    path: dst.path.to_string_lossy().to_string(),
                    enabled: dst.enabled,
                    latest_closed_cycle_id: latest,
                    target_cycle_id: target,
                    last_verified_cycle_id: offset.last_verified_cycle_id,
                    last_completed_cycle_id: offset.last_completed_cycle_id,
                    status: computed_status,
                    status_reason: computed_reason,
                    updated_at: Some(offset.updated_at),
                    issues: self.destination_issues(&source.id, &dst.id)?,
                    scan_differences,
                    scan_at,
                    restart_notice_at: restart_notice_at.clone(),
                    restart_gap_started: restart_gap_started.clone(),
                });
            }
        }
        Ok(views)
    }

    /// Delete event rows for cycles every destination has already verified
    /// past; those cycles can never be re-driven, so their events are dead
    /// weight (realtime sources append events continuously and nothing else
    /// removes them). `keep_from_cycle` is the minimum last-verified cycle
    /// across the source's destinations; the caller passes `None` to skip
    /// pruning (e.g. a destination that has never verified). Chunked so a
    /// large first prune cannot starve other writers.
    pub fn prune_event_log(&self, source_id: &str, keep_from_cycle: i64) -> Result<usize> {
        let sql = format!(
            "DELETE FROM event_log WHERE rowid IN \
             (SELECT rowid FROM event_log WHERE source_id=?1 AND cycle_id<?2 \
              LIMIT {EVENT_PRUNE_CHUNK})"
        );
        let mut total = 0_usize;
        loop {
            let removed = {
                let _write = DB_WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
                self.conn
                    .execute(&sql, params![source_id, keep_from_cycle])?
            };
            total += removed;
            if removed < EVENT_PRUNE_CHUNK {
                return Ok(total);
            }
        }
    }

    fn destination_issues(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<Vec<DestinationIssueView>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT cycle_id, rel_path, issue_kind, message, updated_at
            FROM destination_issue
            WHERE source_id=?1 AND destination_id=?2
            ORDER BY updated_at DESC, rel_path ASC
            "#,
        )?;
        let rows = stmt.query_map(params![source_id, destination_id], |row| {
            Ok(DestinationIssueView {
                cycle_id: row.get(0)?,
                rel_path: row.get(1)?,
                issue_kind: row.get(2)?,
                message: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsUsnCursor {
    pub journal_id: u64,
    pub next_usn: i64,
}

#[derive(Debug, Clone)]
pub struct DestinationOffset {
    pub target_cycle_id: Option<i64>,
    pub last_completed_cycle_id: Option<i64>,
    pub last_verified_cycle_id: Option<i64>,
    pub status: String,
    pub status_reason: String,
    pub updated_at: String,
}

impl State {
    pub fn destination_offset(
        &self,
        source_id: &str,
        destination_id: &str,
    ) -> Result<DestinationOffset> {
        let row = self
            .conn
            .query_row(
                r#"
                SELECT target_cycle_id, last_completed_cycle_id, last_verified_cycle_id,
                       status, status_reason, updated_at
                FROM destination_offset
                WHERE source_id=?1 AND destination_id=?2
                "#,
                params![source_id, destination_id],
                |row| {
                    Ok(DestinationOffset {
                        target_cycle_id: row.get(0)?,
                        last_completed_cycle_id: row.get(1)?,
                        last_verified_cycle_id: row.get(2)?,
                        status: row.get(3)?,
                        status_reason: row.get(4)?,
                        updated_at: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row.unwrap_or(DestinationOffset {
            target_cycle_id: None,
            last_completed_cycle_id: None,
            last_verified_cycle_id: None,
            status: "red".to_string(),
            status_reason: "not_verified".to_string(),
            updated_at: now_string(),
        }))
    }
}

impl State {
    pub fn windows_usn_cursor(
        &self,
        source_id: &str,
        volume: &str,
    ) -> Result<Option<WindowsUsnCursor>> {
        let row: Option<(String, i64)> = self
            .conn
            .query_row(
                r#"
                SELECT journal_id, next_usn
                FROM windows_usn_cursor
                WHERE source_id=?1 AND volume=?2
                "#,
                params![source_id, volume],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(journal_id, next_usn)| {
            Ok(WindowsUsnCursor {
                journal_id: journal_id
                    .parse()
                    .with_context(|| format!("invalid persisted USN journal id {journal_id}"))?,
                next_usn,
            })
        })
        .transpose()
    }

    pub fn set_windows_usn_cursor(
        &self,
        source_id: &str,
        volume: &str,
        journal_id: u64,
        next_usn: i64,
    ) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO windows_usn_cursor
                (source_id, volume, journal_id, next_usn, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(source_id) DO UPDATE SET
                volume=excluded.volume,
                journal_id=excluded.journal_id,
                next_usn=excluded.next_usn,
                updated_at=excluded.updated_at
            "#,
            params![
                source_id,
                volume,
                journal_id.to_string(),
                next_usn,
                now_string()
            ],
        )?;
        Ok(())
    }
}

fn cycle_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Cycle> {
    let starts_at: String = row.get(2)?;
    let ends_at: Option<String> = row.get(3)?;
    Ok(Cycle {
        id: row.get(0)?,
        source_id: row.get(1)?,
        starts_at: parse_db_time(&starts_at).map_err(to_sql_err)?,
        ends_at: ends_at
            .as_deref()
            .map(parse_db_time)
            .transpose()
            .map_err(to_sql_err)?,
        status: row.get(4)?,
        needs_full_rescan: row.get::<_, i64>(5)? != 0,
        manual_full_rescan: row.get::<_, i64>(6)? != 0,
        manual_changed_since_rescan: row.get::<_, i64>(7)? != 0,
    })
}

/// Hash of the config fields [`State::ensure_config`] persists (source ids,
/// paths, enabled flags, destination ids/paths/enabled). Serialization order
/// is stable, so equal configs hash equal.
fn config_fingerprint(cfg: &AppConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for source in &cfg.source_groups {
        source.id.hash(&mut hasher);
        source.src.hash(&mut hasher);
        source.enabled.hash(&mut hasher);
        for dst in &source.destinations {
            dst.id.hash(&mut hasher);
            dst.path.hash(&mut hasher);
            dst.enabled.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn parse_db_time(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid db timestamp {value}"))?
        .with_timezone(&Utc))
}

fn to_sql_err(err: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(err.to_string())),
    )
}

fn bool_to_int(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn now_string() -> String {
    Utc::now().to_rfc3339()
}

pub fn require_existing_config_db(path: &Path) -> Result<State> {
    if !path.exists() {
        return Err(anyhow!("state db does not exist: {}", path.display()));
    }
    State::open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::{
        AppConfig, DestinationConfig, ScheduleConfig, SnapshotConfig, SourceGroupConfig, SyncMode,
    };
    use std::path::PathBuf;

    #[test]
    fn restart_notice_persists_until_covered_or_dismissed() {
        let temp = temp_dir("state_restart_notice");
        let state = State::open(&temp.join("state.sqlite")).unwrap();

        // An action that STARTED BEFORE the notice cannot vouch for the gap.
        let before_notice = Utc::now().to_rfc3339();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(state.raise_restart_notice("src_n").unwrap());
        let (noticed_at, _gap) = state.restart_notice("src_n").unwrap().unwrap();
        assert!(
            !state
                .clear_restart_notice_if_covered("src_n", &before_notice)
                .unwrap(),
            "pre-notice action must not clear the notice"
        );

        // Raising again keeps the original notice (gap never shrinks).
        assert!(!state.raise_restart_notice("src_n").unwrap());
        let (still_at, _) = state.restart_notice("src_n").unwrap().unwrap();
        assert_eq!(still_at, noticed_at);

        // An action started after the notice clears it.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let after_notice = Utc::now().to_rfc3339();
        assert!(
            state
                .clear_restart_notice_if_covered("src_n", &after_notice)
                .unwrap()
        );
        assert!(state.restart_notice("src_n").unwrap().is_none());

        // Manual dismissal clears unconditionally.
        assert!(state.raise_restart_notice("src_n").unwrap());
        state.dismiss_restart_notice("src_n").unwrap();
        assert!(state.restart_notice("src_n").unwrap().is_none());

        std::fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn task_log_tracks_running_tasks_and_prunes_finished_ones_lazily() {
        let temp = temp_dir("state_task_log");
        let state = State::open(&temp.join("state.sqlite")).unwrap();

        let running = state.task_start("compare", "src_a", "dst_a").unwrap();
        let tasks = state.recent_tasks(10).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, "running");
        assert!(tasks[0].ended_at.is_none(), "running task queryable live");

        state
            .task_finish(running, "success", "", 12, 3, 4000)
            .unwrap();
        let tasks = state.recent_tasks(10).unwrap();
        assert_eq!(tasks[0].status, "success");
        assert_eq!(tasks[0].files_synced, 12);
        assert_eq!(tasks[0].differences, 3);
        assert_eq!(tasks[0].entries_scanned, 4000);
        assert!(tasks[0].ended_at.is_some());
        assert!(tasks[0].duration_ms.is_some());

        // Lazy retention: a long-running task survives the prune even while
        // finished rows beyond the newest 100 are dropped at insert time.
        let long_running = state.task_start("sync", "src_a", "dst_a").unwrap();
        for i in 0..120 {
            let id = state.task_start("sync", "src_a", "dst_a").unwrap();
            state.task_finish(id, "success", "", i, 0, 0).unwrap();
        }
        let tasks = state.recent_tasks(200).unwrap();
        assert!(tasks.len() <= 101, "finished rows capped at 100 + running");
        assert!(
            tasks.iter().any(|task| task.id == long_running),
            "running row never pruned"
        );

        // A restart sweep closes orphaned running rows.
        assert_eq!(state.abort_stale_running_tasks().unwrap(), 1);
        let tasks = state.recent_tasks(200).unwrap();
        let orphan = tasks.iter().find(|task| task.id == long_running).unwrap();
        assert_eq!(orphan.status, "aborted");

        // A no-op row can be discarded entirely.
        let noop = state.task_start("sync", "src_a", "dst_a").unwrap();
        state.task_discard(noop).unwrap();
        assert!(
            !state
                .recent_tasks(200)
                .unwrap()
                .iter()
                .any(|task| task.id == noop)
        );

        std::fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn realtime_does_not_advance_only_because_reconcile_interval_elapsed() {
        // Realtime destinations advance on events only; drift repair is the
        // user's explicit Full sync (which compares both trees and transfers
        // just the differences), not a background timer.
        let temp = temp_dir("state_realtime_no_reconcile");
        let db = temp.join("state.sqlite");
        let src = temp.join("src");
        let dst = temp.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups = vec![SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src,
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                reconcile_interval_secs: 1,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        }];

        let state = State::open(&db).unwrap();
        state.ensure_config(&cfg).unwrap();
        state
            .ensure_open_cycle(
                "src_1",
                Utc::now() - chrono::Duration::try_seconds(10).unwrap(),
            )
            .unwrap();
        state
            .upsert_destination_status("src_1", "dst_1", Some(1), "green", "verified")
            .unwrap();

        assert!(
            state
                .advance_due_destination_targets(&cfg)
                .unwrap()
                .is_empty()
        );

        state
            .record_event("src_1", 0, "usn_cursor_reconcile", None, false)
            .unwrap();
        assert!(
            state
                .advance_due_destination_targets(&cfg)
                .unwrap()
                .is_empty()
        );

        state
            .record_event("src_1", 0, "modify", Some("file.txt"), false)
            .unwrap();
        assert_eq!(
            state.advance_due_destination_targets(&cfg).unwrap().len(),
            1
        );

        std::fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn events_close_the_cycle_even_when_no_destination_is_due() {
        // A source whose only destination is on a far-off weekly schedule
        // must still advance its cycle when events accumulate (the UI shows
        // "latest" moving ahead of "verified"); the destination gets NO
        // target — transfer waits for the schedule.
        let temp = temp_dir("state_close_without_due_dst");
        let db = temp.join("state.sqlite");
        let src = temp.join("src");
        let dst = temp.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();

        let far_weekday = (Utc::now() + chrono::Duration::try_days(3).unwrap())
            .format("%A")
            .to_string()
            .to_lowercase();
        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups = vec![SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src,
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig {
                    mode: crate::core::config::ScheduleMode::Weekly,
                    time: "19:00".to_string(),
                    timezone: String::new(),
                    weekday: Some(far_weekday),
                    sync_current_cycle_manually: false,
                },
                paused: false,
                sync: None,
            }],
        }];

        let state = State::open(&db).unwrap();
        state.ensure_config(&cfg).unwrap();
        state.ensure_open_cycle("src_1", Utc::now()).unwrap();
        // Not a first sync: the destination has verified cycle 1 already.
        state
            .upsert_destination_status("src_1", "dst_1", Some(1), "green", "verified")
            .unwrap();

        // No events: nothing closes.
        assert!(
            state
                .advance_due_destination_targets(&cfg)
                .unwrap()
                .is_empty()
        );

        state
            .record_event("src_1", 0, "modify", Some("file.txt"), false)
            .unwrap();
        // Burst not quiesced yet (event just landed): still nothing closes.
        assert!(
            state
                .advance_due_destination_targets(&cfg)
                .unwrap()
                .is_empty()
        );

        // Backdate the last-event marker past the quiescence window.
        state
            .conn
            .execute(
                "UPDATE source_meta SET last_event_observed_at=?1 WHERE source_id='src_1'",
                params![(Utc::now() - chrono::Duration::try_seconds(30).unwrap()).to_rfc3339()],
            )
            .unwrap();

        let closed = state.advance_due_destination_targets(&cfg).unwrap();
        assert_eq!(closed.len(), 1, "cycle closes once events quiesce");
        // The destination was NOT targeted: transfer waits for its schedule.
        let offset = state.destination_offset("src_1", "dst_1").unwrap();
        assert_eq!(offset.target_cycle_id, None);
        // The source's latest cycle advanced past the destination's verified.
        let latest = state.latest_closed_cycle_id("src_1").unwrap();
        assert_eq!(latest, Some(closed[0].id));

        std::fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn verified_cycle_keeps_events_for_lagging_scheduled_destinations() {
        // A verified cycle must NOT eagerly delete its events: a scheduled
        // destination accumulates them across cycles and applies the backlog
        // at its schedule time. Cleanup happens via prune_event_log once every
        // enabled destination has verified past the cycle.
        let temp = temp_dir("state_keep_cycle_events");
        let db = temp.join("state.sqlite");
        let state = State::open(&db).unwrap();
        let cycle_id = state.ensure_open_cycle("src_1", Utc::now()).unwrap();
        state
            .record_event("src_1", 0, "startup_mtime_scan", Some("a.txt"), false)
            .unwrap();
        state
            .record_event("src_1", 0, "modify", Some("b.txt"), false)
            .unwrap();

        state.mark_cycle_status(cycle_id, "verified").unwrap();
        assert_eq!(state.cycle_events("src_1", cycle_id).unwrap().len(), 2);

        // The accumulated backlog spans cycles: events after a destination's
        // last verified cycle up to (including) its target cycle.
        let backlog = state
            .events_between_cycles("src_1", cycle_id - 1, cycle_id)
            .unwrap();
        assert_eq!(backlog.len(), 2);
        assert!(
            state
                .events_between_cycles("src_1", cycle_id, cycle_id)
                .unwrap()
                .is_empty()
        );

        std::fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn prune_event_log_drops_old_cycles_but_keeps_latest_observed_at() {
        let temp = temp_dir("state_prune_events");
        let db = temp.join("state.sqlite");
        let state = State::open(&db).unwrap();

        let first_cycle = state.ensure_open_cycle("src_1", Utc::now()).unwrap();
        state
            .record_event("src_1", 0, "modify", Some("a.txt"), false)
            .unwrap();
        let observed = state.latest_event_observed_at("src_1").unwrap();
        assert!(observed.is_some());
        state.close_current_cycle_for_source("src_1").unwrap();
        let second_cycle = state.ensure_open_cycle("src_1", Utc::now()).unwrap();
        state
            .record_event("src_1", 0, "modify", Some("b.txt"), false)
            .unwrap();

        // Everything verified through the first cycle: its events can go.
        let removed = state.prune_event_log("src_1", second_cycle).unwrap();
        assert_eq!(removed, 1);
        assert!(state.cycle_events("src_1", first_cycle).unwrap().is_empty());
        assert_eq!(state.cycle_events("src_1", second_cycle).unwrap().len(), 1);
        // The startup-gap scalar survives pruning.
        assert!(state.latest_event_observed_at("src_1").unwrap().is_some());

        std::fs::remove_dir_all(temp).ok();
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
