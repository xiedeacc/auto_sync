use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::core::config::AppConfig;
use crate::core::scheduler;

#[derive(Debug, Clone)]
pub struct Cycle {
    pub id: i64,
    pub source_id: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: Option<DateTime<Utc>>,
    pub status: String,
    pub needs_full_rescan: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub rel_path: String,
    pub file_type: String,
    pub size: i64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub hash: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DestinationView {
    pub source_id: String,
    pub destination_id: String,
    pub path: String,
    pub enabled: bool,
    pub latest_closed_cycle_id: Option<i64>,
    pub last_verified_cycle_id: Option<i64>,
    pub status: String,
    pub status_reason: String,
    pub updated_at: Option<String>,
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
            "#,
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
                self.upsert_destination_status(&source.id, &dst.id, None, "red", "not_verified")?;
            }
        }
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
            let due = force || scheduler::cycle_is_due(cycle.starts_at, now, &cfg.schedule);
            if !due {
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

    pub fn current_open_cycle(&self, source_id: &str) -> Result<Option<Cycle>> {
        self.conn
            .query_row(
                r#"
                SELECT id, source_id, starts_at, ends_at, status, needs_full_rescan
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
            SELECT id, source_id, starts_at, ends_at, status, needs_full_rescan
            FROM sync_cycle
            WHERE source_id=?1 AND status IN ('closed', 'planning', 'syncing', 'failed')
            ORDER BY id ASC
            "#,
        )?;
        let rows = stmt.query_map(params![source_id], cycle_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
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

    pub fn mark_cycle_status(&self, cycle_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sync_cycle SET status=?1, updated_at=?2 WHERE id=?3",
            params![status, now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn mark_open_cycle_needs_rescan(&self, source_id: &str) -> Result<()> {
        let cycle_id = self.ensure_open_cycle(source_id, Utc::now())?;
        self.conn.execute(
            "UPDATE sync_cycle SET needs_full_rescan=1, updated_at=?1 WHERE id=?2",
            params![now_string(), cycle_id],
        )?;
        Ok(())
    }

    pub fn record_event(
        &self,
        source_id: &str,
        raw_mask: u64,
        event_kind: &str,
        rel_path: Option<&str>,
        rescan_required: bool,
    ) -> Result<i64> {
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
                (source_id, destination_id, last_verified_cycle_id, status, status_reason, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(source_id, destination_id) DO UPDATE SET
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
                let row = self
                    .conn
                    .query_row(
                        r#"
                        SELECT last_verified_cycle_id, status, status_reason, updated_at
                        FROM destination_offset
                        WHERE source_id=?1 AND destination_id=?2
                        "#,
                        params![source.id, dst.id],
                        |row| {
                            Ok((
                                row.get::<_, Option<i64>>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                let (last_verified, stored_status, reason, updated_at) = row.unwrap_or((
                    None,
                    "red".to_string(),
                    "not_verified".to_string(),
                    now_string(),
                ));

                let computed_status = if latest.is_some() && last_verified >= latest {
                    stored_status
                } else {
                    "red".to_string()
                };
                let computed_reason = if latest.is_some() && last_verified < latest {
                    "behind_latest_cycle".to_string()
                } else {
                    reason
                };
                views.push(DestinationView {
                    source_id: source.id.clone(),
                    destination_id: dst.id.clone(),
                    path: dst.path.to_string_lossy().to_string(),
                    enabled: dst.enabled,
                    latest_closed_cycle_id: latest,
                    last_verified_cycle_id: last_verified,
                    status: computed_status,
                    status_reason: computed_reason,
                    updated_at: Some(updated_at),
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
