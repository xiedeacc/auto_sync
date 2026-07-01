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
    pub differences: Vec<ScanDiffEntry>,
    pub truncated: bool,
    #[serde(default)]
    pub error: String,
}

pub struct State {
    conn: Connection,
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
        let state = Self { conn };
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

            CREATE TABLE IF NOT EXISTS path_snapshot (
                cycle_id INTEGER NOT NULL,
                source_id TEXT NOT NULL,
                rel_path TEXT NOT NULL,
                file_type TEXT NOT NULL,
                size INTEGER NOT NULL,
                mtime_ns INTEGER NOT NULL,
                mode INTEGER NOT NULL,
                hash TEXT,
                PRIMARY KEY (cycle_id, source_id, rel_path)
            );

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
        if let Some(id) = self.current_open_cycle_id(source_id)? {
            return Ok(id);
        }
        let now = starts_at.to_rfc3339();
        self.conn.execute(
            r#"
            INSERT INTO sync_cycle
                (source_id, starts_at, status, needs_full_rescan, created_at, updated_at)
            VALUES (?1, ?2, 'open', 0, ?2, ?2)
            "#,
            params![source_id, now],
        )?;
        Ok(self.conn.last_insert_rowid())
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

            for dst in source.destinations.iter().filter(|d| d.enabled) {
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
            for dst in source.destinations.iter().filter(|d| d.enabled) {
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
        for dst in source.destinations.iter().filter(|d| d.enabled) {
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

    pub fn latest_event_observed_at(&self, source_id: &str) -> Result<Option<DateTime<Utc>>> {
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
        self.conn.execute(
            "UPDATE sync_cycle SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, now_string(), cycle_id],
        )?;
        if status == "verified" {
            self.delete_cycle_startup_mtime_events(cycle_id)?;
        }
        Ok(())
    }

    fn delete_cycle_startup_mtime_events(&self, cycle_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM event_log WHERE cycle_id=?1 AND event_kind='startup_mtime_scan'",
            params![cycle_id],
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

    pub fn mark_cycle_manual_changed_since_rescan(&self, cycle_id: i64) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE sync_cycle
            SET manual_changed_since_rescan=1, updated_at=?1
            WHERE id=?2
            "#,
            params![now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn clear_cycle_needs_rescan(&self, cycle_id: i64) -> Result<()> {
        self.conn.execute(
            r#"
            UPDATE sync_cycle
            SET needs_full_rescan=0,
                manual_full_rescan=0,
                manual_changed_since_rescan=0,
                updated_at=?1
            WHERE id=?2
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
        if rescan_required {
            self.mark_open_cycle_needs_rescan(source_id)?;
        }
        Ok(self.conn.last_insert_rowid())
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
                });
            }
        }
        Ok(views)
    }

    pub fn replace_snapshot(
        &mut self,
        cycle_id: i64,
        source_id: &str,
        entries: &[SnapshotEntry],
    ) -> Result<()> {
        let _write = DB_WRITE_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM path_snapshot WHERE cycle_id=?1 AND source_id=?2",
            params![cycle_id, source_id],
        )?;
        {
            let mut stmt = tx.prepare(
                r#"
                INSERT INTO path_snapshot
                    (cycle_id, source_id, rel_path, file_type, size, mtime_ns, mode, hash)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                "#,
            )?;
            for entry in entries {
                stmt.execute(params![
                    cycle_id,
                    source_id,
                    entry.rel_path,
                    entry.file_type,
                    entry.size,
                    entry.mtime_ns,
                    entry.mode as i64,
                    entry.hash
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn snapshot_count(&self, cycle_id: i64, source_id: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM path_snapshot WHERE cycle_id=?1 AND source_id=?2",
                params![cycle_id, source_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub fn snapshot_entries(&self, cycle_id: i64, source_id: &str) -> Result<Vec<SnapshotEntry>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT rel_path, file_type, size, mtime_ns, mode, hash
            FROM path_snapshot
            WHERE cycle_id=?1 AND source_id=?2
            ORDER BY rel_path ASC
            "#,
        )?;
        let rows = stmt.query_map(params![cycle_id, source_id], |row| {
            Ok(SnapshotEntry {
                rel_path: row.get(0)?,
                file_type: row.get(1)?,
                size: row.get(2)?,
                mtime_ns: row.get(3)?,
                mode: row.get::<_, i64>(4)? as u32,
                hash: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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
    fn realtime_does_not_advance_only_because_reconcile_interval_elapsed() {
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
    fn verified_cycle_deletes_only_startup_mtime_events() {
        let temp = temp_dir("state_delete_startup_events");
        let db = temp.join("state.sqlite");
        let state = State::open(&db).unwrap();
        let cycle_id = state.ensure_open_cycle("src_1", Utc::now()).unwrap();
        state
            .record_event("src_1", 0, "startup_mtime_scan", Some("a.txt"), false)
            .unwrap();
        state
            .record_event("src_1", 0, "modify", Some("b.txt"), false)
            .unwrap();

        state.mark_cycle_status(cycle_id, "failed").unwrap();
        assert_eq!(state.cycle_events("src_1", cycle_id).unwrap().len(), 2);

        state.mark_cycle_status(cycle_id, "verified").unwrap();
        let events = state.cycle_events("src_1", cycle_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_kind, "modify");

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
