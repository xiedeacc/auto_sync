use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
#[cfg(windows)]
use std::os::windows::fs::{symlink_dir, symlink_file};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use filetime::{FileTime, set_file_mtime};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::core::cancel;
use crate::core::config::{
    AppConfig, DEFAULT_MAX_PARALLEL_TRANSFERS, DEFAULT_TRANSFER_TIMEOUT_SECS, DestinationConfig,
    NativeSyncConfig, SnapshotBackend, SourceGroupConfig, machine_id_or_local, machine_is_local,
};
use crate::core::machines::{
    configure_tcp_connection_pool, encode_query_component, find_machine, remote_get_json,
    remote_post_bytes, remote_post_json,
};
use crate::core::progress;
use crate::core::state::{Cycle, CycleEvent, ScanDiffEntry, ScanReport, SnapshotEntry, State};
use crate::core::status::{check_destination_online, check_file_destination_online};
use crate::core::storage;

pub mod delta;

const INTERNAL_TMP: &str = ".auto_sync_tmp";
const INTERNAL_TRASH: &str = ".auto_sync_trash";
const INTERNAL_PROBE: &str = ".auto_sync_probe";
const TRANSFER_CHUNK_SIZE: usize = 16 * 1024 * 1024;
/// Files at least this large that already exist on the destination are sent as
/// an rsync-style delta (only changed regions) instead of being re-sent whole.
const DELTA_MIN_SIZE: u64 = 256 * 1024;
/// Upper bound on files eligible for delta. The sender still buffers the new
/// file (and the encoded delta) in memory, so this is kept bounded to limit peak
/// RAM under parallel transfers; larger changed files use the chunked streaming
/// path (16 MiB buffer) instead. The receiver basis is read as a stream.
const DELTA_MAX_SIZE: u64 = 512 * 1024 * 1024;
/// Cap on the total bytes of whole-file buffers held concurrently by transfer
/// workers (the delta sender buffers file + encoded delta, ~2x file size). A
/// worker over budget waits until others release; a single request larger than
/// the whole budget is still allowed to run alone so it cannot deadlock.
const TRANSFER_MEMORY_BUDGET: u64 = 1024 * 1024 * 1024;

/// Serializes every run of the sync engine within a process. With the daemon,
/// web server and (optional) desktop UI now sharing one process, the scheduled
/// tick and a manually triggered sync must never drive the engine concurrently.
static SYNC_GATE: OnceLock<Mutex<()>> = OnceLock::new();
static SCAN_GATE: OnceLock<Mutex<()>> = OnceLock::new();
static SYNC_KIND: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// Coarse phase of the pass currently holding the sync gate (e.g. "zfs diff",
/// "scanning", "transferring", "verifying"). Surfaced in the UI Info panel so a
/// long-running task shows WHAT it is doing right now, not just that it is busy.
static SYNC_PHASE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
/// Live file-count progress (total / to-copy / matched / done) of the running
/// pass; see [`SyncPlan`].
static SYNC_PLAN: OnceLock<Mutex<Option<Arc<SyncPlan>>>> = OnceLock::new();
static TRANSFER_MEMORY: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

fn transfer_memory() -> &'static (Mutex<u64>, Condvar) {
    TRANSFER_MEMORY.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

/// Permit for `bytes` of in-memory transfer buffer, released on drop.
struct TransferMemoryPermit {
    bytes: u64,
}

impl Drop for TransferMemoryPermit {
    fn drop(&mut self) {
        let (used, available) = transfer_memory();
        let mut used = used.lock().unwrap_or_else(|err| err.into_inner());
        *used = used.saturating_sub(self.bytes);
        available.notify_all();
    }
}

/// Block until `bytes` fit under [`TRANSFER_MEMORY_BUDGET`], then reserve them.
/// A request larger than the whole budget proceeds once nothing else holds a
/// permit, so oversized files degrade to serialized rather than deadlocking.
fn acquire_transfer_memory(bytes: u64) -> TransferMemoryPermit {
    let (used, available) = transfer_memory();
    let mut used = used.lock().unwrap_or_else(|err| err.into_inner());
    while *used > 0 && used.saturating_add(bytes) > TRANSFER_MEMORY_BUDGET {
        used = available.wait(used).unwrap_or_else(|err| err.into_inner());
    }
    *used = used.saturating_add(bytes);
    TransferMemoryPermit { bytes }
}

struct SyncKindGuard {
    previous: Option<String>,
}

impl Drop for SyncKindGuard {
    fn drop(&mut self) {
        let mut kind = sync_kind_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        *kind = self.previous.take();
    }
}

pub fn sync_gate() -> &'static Mutex<()> {
    SYNC_GATE.get_or_init(|| Mutex::new(()))
}

/// A separate lock for Scan (dry-run compare). Scan is read-only and must NOT
/// block the real backup, so it does not take [`sync_gate`]; this only prevents
/// two scans of the same process from overlapping.
fn scan_gate() -> &'static Mutex<()> {
    SCAN_GATE.get_or_init(|| Mutex::new(()))
}

const SCAN_ALREADY_RUNNING: &str = "a compare is already in progress";

/// True when `err` is the scan gate's concurrent-run rejection; callers use
/// this to avoid overwriting the running scan's report with a failure record.
pub fn scan_error_is_already_running(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(SCAN_ALREADY_RUNNING))
}

pub fn sync_is_running() -> bool {
    sync_gate().try_lock().is_err()
}

pub fn current_sync_kind() -> Option<String> {
    sync_kind_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn sync_kind_lock() -> &'static Mutex<Option<String>> {
    SYNC_KIND.get_or_init(|| Mutex::new(None))
}

pub fn current_sync_phase() -> Option<String> {
    sync_phase_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn sync_phase_lock() -> &'static Mutex<Option<String>> {
    SYNC_PHASE.get_or_init(|| Mutex::new(None))
}

/// Record the coarse phase of the running pass (plain setter; overwrites).
/// `SyncPhaseReset` clears it when the top-level pass unwinds so it never
/// sticks after work stops.
fn set_sync_phase(phase: &str) {
    *sync_phase_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(phase.to_string());
}

/// Live file-count progress for the pass currently holding the sync gate. Set
/// once the reconcile plan is known (after the two-tree diff), so the UI can
/// show "synced X / Y to copy · Z unchanged (N total)" during a long Full
/// instead of a bare "syncing…". `done` is bumped by the transfer workers.
/// A plain global (like `SYNC_PHASE`) so `runtime_status` can read it off the
/// web thread; cleared by `SyncPhaseReset` when the pass unwinds.
struct SyncPlan {
    total: u64,
    to_copy: u64,
    matched: u64,
    done: AtomicU64,
}

fn sync_plan_lock() -> &'static Mutex<Option<Arc<SyncPlan>>> {
    SYNC_PLAN.get_or_init(|| Mutex::new(None))
}

fn set_sync_plan(total: u64, to_copy: u64, matched: u64) {
    *sync_plan_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner()) = Some(Arc::new(SyncPlan {
        total,
        to_copy,
        matched,
        done: AtomicU64::new(0),
    }));
}

fn sync_plan_add_done(count: u64) {
    if let Some(plan) = sync_plan_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .as_ref()
    {
        plan.done.fetch_add(count, Ordering::Relaxed);
    }
}

/// `(total, to_copy, matched, done)` for the running pass, if a plan is set.
pub fn current_sync_plan() -> Option<(u64, u64, u64, u64)> {
    sync_plan_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .as_ref()
        .map(|plan| {
            (
                plan.total,
                plan.to_copy,
                plan.matched,
                plan.done.load(Ordering::Relaxed),
            )
        })
}

struct SyncPhaseReset;

impl SyncPhaseReset {
    fn enter() -> Self {
        SyncPhaseReset
    }
}

impl Drop for SyncPhaseReset {
    fn drop(&mut self) {
        *sync_phase_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = None;
        *sync_plan_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner()) = None;
    }
}

fn set_sync_kind(kind: &str) -> SyncKindGuard {
    let mut current = sync_kind_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let previous = current.replace(kind.to_string());
    SyncKindGuard { previous }
}

fn set_sync_kind_if_empty(kind: &str) -> SyncKindGuard {
    let mut current = sync_kind_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let previous = current.clone();
    if current.is_none() {
        *current = Some(kind.to_string());
    }
    SyncKindGuard { previous }
}

pub fn sync_all_pending(cfg: &AppConfig, state: &mut State) -> Result<()> {
    let _serialized = sync_gate().lock().unwrap_or_else(|err| err.into_inner());
    // A scheduler-driven pass (no explicit kind set by a manual entry point) is
    // an event-replay incremental by default; a per-cycle refinement below
    // promotes it to "full" when it actually re-walks the whole tree. Kept as a
    // single concrete kind — no "automatic" alias — so the task log is
    // unambiguous.
    let _kind = set_sync_kind_if_empty("incremental");
    // Clear the phase indicator when the whole pass unwinds so it never sticks
    // after work stops (even on early error).
    let _phase_reset = SyncPhaseReset::enter();
    let _cancellable = cancel::begin(cancel::KIND_SYNC);
    sync_all_pending_inner(cfg, state)
}

/// Record a manual sync request for one destination WITHOUT running the
/// engine: close the open cycle, target this destination, and set the mode
/// flags. Plain DB writes — safe while a sync is running; the engine (or the
/// next scheduler tick) picks the target up on its own, so a busy engine
/// queues the request instead of failing it with "sync already in progress".
pub fn queue_destination_sync(
    cfg: &AppConfig,
    state: &State,
    source_id: &str,
    destination_id: &str,
    mode: SyncRequestMode,
) -> Result<()> {
    let paused = cfg
        .source_groups
        .iter()
        .find(|source| source.id == source_id)
        .and_then(|source| {
            source
                .destinations
                .iter()
                .find(|dst| dst.id == destination_id)
        })
        .is_some_and(|dst| dst.paused);
    if paused {
        bail!("destination is paused; resume it before syncing");
    }
    if matches!(mode, SyncRequestMode::RepairScan) {
        return queue_scan_repair(cfg, state, source_id, destination_id);
    }
    if let Some(cycle) = state.force_target_destination(cfg, source_id, destination_id)? {
        match mode {
            SyncRequestMode::Incremental => {}
            SyncRequestMode::Full => state.mark_cycle_manual_full_rescan(cycle.id)?,
            SyncRequestMode::RepairScan => unreachable!("handled above"),
        }
    }
    Ok(())
}

/// Queue a repair of exactly the differences the last Compare reported:
/// inject the report's paths as synthetic events and target the destination
/// with a plain incremental, which applies precisely those paths (copying
/// only entries that actually differ, mirror-deleting destination extras).
/// When the stored report is a truncated sample — it cannot name every
/// difference — escalate to a full reconcile instead. The consumed report is
/// deleted either way so the UI's repair affordance clears; a fresh Compare
/// re-establishes it.
fn queue_scan_repair(
    cfg: &AppConfig,
    state: &State,
    source_id: &str,
    destination_id: &str,
) -> Result<()> {
    let Some(report) = state.get_scan_report(source_id, destination_id)? else {
        bail!("no compare report stored; run Compare first");
    };
    if !report.error.is_empty() {
        bail!("last compare failed ({}); run Compare again", report.error);
    }
    let total = report.to_add
        + report.to_update
        + report.to_delete
        + report.type_mismatch
        + report.metadata;
    if total == 0 {
        return Ok(());
    }
    let sample_is_complete = !report.truncated && report.differences.len() as u64 >= total;
    if sample_is_complete {
        for diff in &report.differences {
            state.record_event(source_id, 0, "scan_repair", Some(&diff.rel_path), false)?;
        }
        state.force_target_destination(cfg, source_id, destination_id)?;
        info!(
            source = source_id,
            destination = destination_id,
            paths = report.differences.len(),
            "queued scan repair for reported differences"
        );
    } else {
        // The report only holds a sample of the differences: repairing just
        // the sample would leave the rest untouched, so reconcile fully.
        if let Some(cycle) = state.force_target_destination(cfg, source_id, destination_id)? {
            state.mark_cycle_manual_full_rescan(cycle.id)?;
        }
        info!(
            source = source_id,
            destination = destination_id,
            total,
            "scan report is a truncated sample; repairing via full reconcile"
        );
    }
    state.delete_scan_report(source_id, destination_id)?;
    Ok(())
}

/// Drive all pending cycles under a manual sync kind, WAITING for a running
/// engine pass to finish instead of failing. Intended for background threads
/// serving a queued manual request.
pub fn run_pending_with_kind(cfg: &AppConfig, state: &mut State, kind: &str) -> Result<()> {
    let _serialized = sync_gate().lock().unwrap_or_else(|err| err.into_inner());
    let _kind = set_sync_kind(kind);
    let _cancellable = cancel::begin(cancel::KIND_SYNC);
    sync_all_pending_inner(cfg, state)
}

fn sync_all_pending_inner(cfg: &AppConfig, state: &mut State) -> Result<()> {
    configure_tcp_connection_pool(cfg.app.tcp_connection_pool_size);
    crate::core::machines::configure_peer_token(&cfg.app.peer_token);
    configure_fsync(cfg.app.sync.fsync);
    progress::configure_progress_file(&cfg.app.data_db);
    state.ensure_config(cfg)?;
    // Rows for deleted (source, destination) pairs pinned their snapshots
    // forever; drop them and best-effort reclaim their dstbase snapshots.
    match state.prune_removed_destination_offsets(cfg) {
        Ok(orphans) => {
            for snapshot in orphans {
                Command::new("zfs")
                    .args(["destroy", &snapshot])
                    .status()
                    .ok();
            }
        }
        Err(err) => warn!(error = %err, "failed to prune removed destination offsets"),
    }
    loop {
        let mut progressed = false;
        let mut blocked = false;
        for source in cfg
            .source_groups
            .iter()
            .filter(|s| s.enabled && machine_id_or_local(&s.machine_id) == "local")
        {
            let mut source_progressed = false;
            let cycles = state.closed_cycles_for_source(&source.id)?;
            for cycle in cycles {
                cancel::check()?;
                if state.source_has_target_cycle(&source.id, cycle.id)? {
                    // Per-source isolation: a persistently failing source
                    // (unplugged disk, poisoned event row) must not starve
                    // every source after it in the config order — only
                    // cancellation stops the pass. The failing source itself
                    // already recorded a red destination status.
                    match sync_cycle_for_source(cfg, state, source, &cycle) {
                        Ok(outcome) => {
                            progressed |= outcome.progressed;
                            source_progressed |= outcome.progressed;
                            blocked |= outcome.blocked;
                        }
                        Err(err) if cancel::error_is_cancelled(&err) => return Err(err),
                        Err(err) => {
                            error!(
                                source = source.id,
                                cycle_id = cycle.id,
                                error = %err,
                                "sync cycle failed; continuing with remaining sources"
                            );
                            break; // next cycles of this source would hit the same error
                        }
                    }
                } else if cycle.status == "closed" {
                    state.mark_cycle_status(cycle.id, "verified")?;
                }
            }
            // Cycles every destination verified past can never be re-driven,
            // so their event rows are dead weight. Only after a pass that did
            // work: pruning every idle scheduler tick burned CPU for nothing.
            if !source_progressed {
                continue;
            }
            if let Err(err) = prune_verified_cycle_events(state, source) {
                warn!(source = source.id, error = %err, "failed to prune event log");
            }
        }
        if !progressed || !blocked {
            break;
        }
    }
    Ok(())
}

/// Delete event rows for cycles that every enabled destination of this source
/// has verified past; they can never be re-driven. Skipped while any
/// destination has never verified (conservative: everything might still
/// matter to its first pass).
fn prune_verified_cycle_events(state: &State, source: &SourceGroupConfig) -> Result<()> {
    let mut min_verified: Option<i64> = None;
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        match state.destination_last_verified(&source.id, &dst.id)? {
            Some(verified) => {
                min_verified = Some(min_verified.map_or(verified, |min| min.min(verified)));
            }
            None => return Ok(()),
        }
    }
    let Some(keep_from) = min_verified else {
        return Ok(());
    };
    let removed = state.prune_event_log(&source.id, keep_from)?;
    if removed > 0 {
        info!(
            source = source.id,
            removed, keep_from, "pruned verified cycle events"
        );
    }
    Ok(())
}

pub fn sync_all_now(cfg: &AppConfig, state: &mut State) -> Result<()> {
    let _kind = set_sync_kind("incremental");
    state.force_target_all_destinations(cfg)?;
    sync_all_pending(cfg, state)
}

pub fn sync_source_now(cfg: &AppConfig, state: &mut State, source_id: &str) -> Result<()> {
    let _kind = set_sync_kind("incremental");
    state.force_target_source(cfg, source_id)?;
    sync_all_pending(cfg, state)
}

pub fn sync_destination_now(
    cfg: &AppConfig,
    state: &mut State,
    source_id: &str,
    destination_id: &str,
) -> Result<()> {
    sync_destination_now_with_mode(
        cfg,
        state,
        source_id,
        destination_id,
        SyncRequestMode::Incremental,
    )
}

pub fn sync_destination_now_with_mode(
    cfg: &AppConfig,
    state: &mut State,
    source_id: &str,
    destination_id: &str,
    mode: SyncRequestMode,
) -> Result<()> {
    let _kind = set_sync_kind(sync_request_mode_wire_value(mode));
    queue_destination_sync(cfg, state, source_id, destination_id, mode)?;
    sync_all_pending(cfg, state)
}

/// Per-kind cap on sampled differing paths kept in the report (the UI shows up
/// to 50 of each kind; the headroom covers the popup without bloating the JSON).
const SCAN_DIFF_PER_KIND_CAP: usize = 200;

/// Dry-run compare of a destination against its source. Reads both trees and
/// reports how they differ (add/update/delete/type-mismatch) WITHOUT changing
/// anything. The result is persisted so the UI info panel can display it.
pub fn scan_destination_now(
    cfg: &AppConfig,
    state: &State,
    source_id: &str,
    destination_id: &str,
) -> Result<ScanReport> {
    // Scan is read-only: it serializes only against other scans, never against
    // the real backup, so a long compare cannot stall syncing.
    let _serialized = scan_gate()
        .try_lock()
        .map_err(|_| anyhow!("{SCAN_ALREADY_RUNNING}"))?;
    // The mirror direction of the sync↔compare courtesy: the engine defers a
    // destination while its compare runs; a compare STARTED mid-sync would
    // read a half-updated tree and report transient false differences.
    if cancel::kind_target_active(
        cancel::KIND_SYNC,
        &cancel::target_for(source_id, destination_id),
    ) {
        bail!("a sync for this destination is running; compare after it finishes");
    }
    let _kind = set_sync_kind_if_empty("scan");
    // Clear the phase indicator when the compare unwinds (zfs_diff_changed_paths
    // below sets it to "zfs diff"; a full-walk compare leaves it unset and the
    // UI derives "scanning" from the live scan progress).
    let _phase_reset = SyncPhaseReset::enter();
    let _cancellable = cancel::begin_target(
        cancel::KIND_COMPARE,
        Some(cancel::target_for(source_id, destination_id)),
    );
    // Tag tree walks started here as compare progress so they do not fight a
    // concurrently running sync's scan for the UI progress display.
    let _compare = progress::enter_compare_context();
    configure_tcp_connection_pool(cfg.app.tcp_connection_pool_size);
    progress::configure_progress_file(&cfg.app.data_db);

    let source = cfg
        .source_groups
        .iter()
        .find(|s| s.id == source_id && s.enabled)
        .ok_or_else(|| anyhow!("source not found or disabled: {source_id}"))?;
    let dst = source
        .destinations
        .iter()
        .find(|d| d.id == destination_id && d.enabled)
        .ok_or_else(|| anyhow!("destination not found or disabled: {destination_id}"))?;
    let sync = effective_sync_config(cfg, dst);
    // Whole-tree snapshot responses arrive only after the peer finishes its
    // walk; the per-file transfer timeout is far too small for that.
    let timeout = snapshot_timeout(&sync);

    let source_machine_id = machine_id_or_local(&source.machine_id);
    let source_machine = machine_or_local(cfg, source_machine_id)?;
    let source_info = path_info_on_machine(source_machine_id, &source_machine, &source.src)?;

    let dst_machine_id = machine_id_or_local(&dst.machine_id);
    let dst_machine = machine_or_local(cfg, dst_machine_id)?;
    let dst_root = destination_root_for_source(source, &source_info, &dst.path, &dst_machine);

    // Fast path: when BOTH sides live on local ZFS datasets with verified
    // baseline snapshots, diff each side against its base and examine only
    // the union of changed paths — everything else was verified in sync at
    // the baselines and untouched since. Any missing precondition or zfs
    // failure falls back to the full two-tree walk below.
    if source_info.kind == "dir" {
        match zfs_diff_compare(state, source, destination_id, &dst_root, &sync) {
            Ok(Some(report)) => {
                state.put_scan_report(&report)?;
                return Ok(report);
            }
            Ok(None) => {}
            Err(err) if cancel::error_is_cancelled(&err) => return Err(err),
            Err(err) => {
                warn!(
                    source = source_id,
                    destination = destination_id,
                    error = %err,
                    "zfs diff compare failed; falling back to full compare"
                );
            }
        }
    }

    // Source and destination trees are independent: scan them concurrently
    // (they usually live on different machines), halving the compare's scan
    // phase versus scanning serially.
    let in_compare = progress::in_compare_context();
    let cancel_token = cancel::current_token();
    let (source_result, dst_result) = thread::scope(|scope| {
        let dst_handle = scope.spawn(|| {
            let _compare = in_compare.then(progress::enter_compare_context);
            let _cancel = cancel::enter(cancel_token.clone());
            if source_info.kind == "dir" {
                snapshot_on_machine(
                    dst_machine_id,
                    &dst_machine,
                    &dst_root,
                    TransferSnapshotMode::Destination,
                    &[],
                    sync.checksum,
                    timeout,
                )
            } else {
                // A file source syncs exactly one destination path and never
                // deletes anything else; snapshot just that path so mirror mode
                // does not report the destination directory's other files as
                // pending deletions.
                snapshot_paths_on_machine(
                    dst_machine_id,
                    &dst_machine,
                    &dst_root,
                    &diff_paths_all_recursive(std::slice::from_ref(&source_info.name)),
                    TransferSnapshotMode::Destination,
                    &[],
                    sync.checksum,
                    timeout,
                )
            }
        });
        let source_result = snapshot_on_machine(
            source_machine_id,
            &source_machine,
            &source_info.base,
            TransferSnapshotMode::Source,
            &source.excludes,
            sync.checksum,
            timeout,
        );
        let dst_result = dst_handle.join().expect("destination scan thread panicked");
        (source_result, dst_result)
    });
    let mut source_snapshot = source_result?;
    let mut dst_snapshot = dst_result?;
    if source_info.kind != "dir" {
        source_snapshot.retain(|entry| entry.rel_path == source_info.name);
        dst_snapshot.retain(|entry| entry.rel_path == source_info.name);
    }

    let report = build_scan_report(
        source_id,
        destination_id,
        &source_snapshot,
        &dst_snapshot,
        &source.excludes,
        &sync,
        false,
    );
    state.put_scan_report(&report)?;
    Ok(report)
}

/// Compare both trees via `zfs diff` against their verified baseline
/// snapshots (see [`record_destination_verified_baselines`]): only the union
/// of paths either side changed since its base needs examining — at the
/// baselines the two sides were verified identical. Returns `Ok(None)` when
/// the fast path does not apply (either side not ZFS-local, no baselines,
/// dataset mismatch, or zfs failed) so the caller runs the full walk.
fn zfs_diff_compare(
    state: &State,
    source: &SourceGroupConfig,
    destination_id: &str,
    dst_root: &Path,
    sync: &NativeSyncConfig,
) -> Result<Option<ScanReport>> {
    if !sync.zfs_diff {
        return Ok(None);
    }
    let Some(src_base) = state.destination_verified_snapshot(&source.id, destination_id)? else {
        return Ok(None);
    };
    let Some(dst_base) = state.destination_verified_dst_snapshot(&source.id, destination_id)?
    else {
        return Ok(None);
    };
    // Both roots must resolve to local ZFS datasets matching the recorded
    // bases: never trust "no diff" as "unchanged" for the wrong dataset.
    let Ok(src_dataset) = resolve_zfs_dataset(source) else {
        return Ok(None);
    };
    let Ok(dst_dataset) = resolve_dataset_for_path(dst_root) else {
        return Ok(None);
    };
    if src_base.split('@').next() != Some(src_dataset.name.as_str())
        || dst_base.split('@').next() != Some(dst_dataset.name.as_str())
    {
        return Ok(None);
    }
    let src_live_root = if src_dataset.path_in_dataset.as_os_str().is_empty() {
        src_dataset.mountpoint.clone()
    } else {
        src_dataset.mountpoint.join(&src_dataset.path_in_dataset)
    };
    let Ok(dst_live_root) = dst_root.canonicalize() else {
        return Ok(None);
    };
    let Some(src_changed) = zfs_diff_changed_paths_live(&src_base, &src_live_root) else {
        return Ok(None);
    };
    let Some(dst_changed) = zfs_diff_changed_paths_live(&dst_base, &dst_live_root) else {
        return Ok(None);
    };
    let (paths, dst_only) = diff_union_and_dst_only(src_changed, dst_changed);
    info!(
        source = source.id,
        destination = destination_id,
        src_base,
        dst_base,
        changed = paths.len(),
        "compare served by zfs diff against verified baselines"
    );
    cancel::check()?;
    let mut source_snapshot = take_snapshot_diff_paths_with_excludes(
        &src_live_root,
        &paths,
        SnapshotMode::Source,
        &source.excludes,
        sync.checksum,
    )?;
    reject_dangerous_destination(&dst_live_root)?;
    let mut dst_snapshot = take_snapshot_diff_paths_with_excludes(
        &dst_live_root,
        &paths,
        SnapshotMode::Destination,
        &[],
        sync.checksum,
    )?;
    if !sync.checksum {
        add_hash_evidence(
            &src_live_root,
            &dst_only,
            SnapshotMode::Source,
            &source.excludes,
            &mut source_snapshot,
        )?;
        add_hash_evidence(
            &dst_live_root,
            &dst_only,
            SnapshotMode::Destination,
            &[],
            &mut dst_snapshot,
        )?;
    }
    let mut report = build_scan_report(
        &source.id,
        destination_id,
        &source_snapshot,
        &dst_snapshot,
        &source.excludes,
        sync,
        true,
    );
    report.method = "zfs_diff".to_string();
    Ok(Some(report))
}

/// Re-snapshot the given paths with hashing enabled and swap the results into
/// `entries`. When both sides of a comparison carry a hash, hash evidence
/// drives [`entries_match_with`] even outside checksum mode.
fn add_hash_evidence(
    root: &Path,
    rel_paths: &[DiffPath],
    mode: SnapshotMode,
    excludes: &[PathBuf],
    entries: &mut Vec<SnapshotEntry>,
) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }
    let hashed = take_snapshot_diff_paths_with_excludes(root, rel_paths, mode, excludes, true)?;
    if hashed.is_empty() {
        return Ok(());
    }
    let mut by_rel: BTreeMap<String, SnapshotEntry> = hashed
        .into_iter()
        .map(|entry| (entry.rel_path.clone(), entry))
        .collect();
    for entry in entries.iter_mut() {
        if let Some(upgraded) = by_rel.remove(&entry.rel_path) {
            *entry = upgraded;
        }
    }
    entries.extend(by_rel.into_values());
    Ok(())
}

fn machine_or_local(
    cfg: &AppConfig,
    machine_id: &str,
) -> Result<crate::core::config::MachineConfig> {
    if let Some(machine) = find_machine(cfg, machine_id) {
        return Ok(machine);
    }
    if machine_id == "local" {
        // snapshot/path-info ignore the machine handle for local roots.
        return Ok(crate::core::config::MachineConfig {
            id: "local".to_string(),
            ..Default::default()
        });
    }
    bail!("unknown machine: {machine_id}")
}

/// The single manifest-comparison result shared by Scan (report), the
/// cross-machine full transfer, and the local full sync — so "what differs"
/// can never diverge between the difference report and the actual repair.
/// Comparison is by relative path (traversal order is irrelevant); files
/// compare size+mtime (or hash in checksum mode), symlinks compare target,
/// and directories only count as different on add/type-mismatch — an
/// mtime-only touch on a directory must not trigger any work.
struct ManifestDiff<'a> {
    /// Files/symlinks to copy: (source entry, existing same-type dst entry
    /// usable as a delta basis).
    transfer: Vec<(&'a SnapshotEntry, Option<&'a SnapshotEntry>)>,
    /// Source entries whose destination path holds a DIFFERENT file type
    /// (must be removed before the source version is written).
    type_mismatch: Vec<&'a SnapshotEntry>,
    /// Source directories missing from the destination.
    missing_dirs: Vec<&'a SnapshotEntry>,
    /// Destination-only entries (mirror delete candidates), excludes applied.
    extras: Vec<&'a SnapshotEntry>,
    /// Content-equal files whose permission bits differ (chmod in place).
    mode_fixes: Vec<&'a SnapshotEntry>,
    /// Source entries already matching on the destination.
    in_sync: u64,
}

/// `exact` selects the evidence-path mtime comparison (see
/// [`entries_match_exact`]): pass `true` when the manifests were built from a
/// positive change list (events, zfs diff) rather than a whole-tree walk.
fn diff_manifests<'a>(
    source_snapshot: &'a [SnapshotEntry],
    dst_snapshot: &'a [SnapshotEntry],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
    exact: bool,
) -> ManifestDiff<'a> {
    // Reference maps: the snapshots can each hold hundreds of thousands of
    // entries, and cloning both into owned maps doubles peak memory.
    let source_map = map_entry_refs(source_snapshot);
    let dst_map = map_entry_refs(dst_snapshot);
    let mut diff = ManifestDiff {
        transfer: Vec::new(),
        type_mismatch: Vec::new(),
        missing_dirs: Vec::new(),
        extras: Vec::new(),
        mode_fixes: Vec::new(),
        in_sync: 0,
    };
    for entry in source_snapshot {
        if is_rel_excluded(Path::new(&entry.rel_path), excludes) {
            continue;
        }
        match dst_map.get(entry.rel_path.as_str()) {
            None => {
                if entry.file_type == "dir" {
                    diff.missing_dirs.push(entry);
                } else {
                    diff.transfer.push((entry, None));
                }
            }
            Some(existing) if existing.file_type != entry.file_type => {
                diff.type_mismatch.push(entry);
            }
            Some(existing) => {
                if entry.file_type == "dir" || entries_match_with(entry, existing, sync, exact) {
                    diff.in_sync += 1;
                    if entry_mode_differs(entry, existing) {
                        diff.mode_fixes.push(entry);
                    }
                } else {
                    diff.transfer.push((entry, Some(existing)));
                }
            }
        }
    }
    for entry in dst_snapshot {
        if is_rel_excluded(Path::new(&entry.rel_path), excludes) {
            continue;
        }
        if !source_map.contains_key(entry.rel_path.as_str()) {
            diff.extras.push(entry);
        }
    }
    diff
}

impl<'a> ManifestDiff<'a> {
    /// The copy work list for a sync: everything in `transfer`, plus the
    /// type-mismatched files/symlinks (their old destination entry is removed
    /// first, so they copy with no delta basis). Directories are not copied —
    /// they are created explicitly.
    fn entries_to_copy(&self) -> Vec<(&'a SnapshotEntry, Option<&'a SnapshotEntry>)> {
        self.transfer
            .iter()
            .copied()
            .chain(
                self.type_mismatch
                    .iter()
                    .filter(|entry| entry.file_type != "dir")
                    .map(|entry| (*entry, None)),
            )
            .collect()
    }

    /// Mirror-delete candidates, deepest paths first.
    fn extra_paths_deepest_first(&self) -> Vec<String> {
        let mut paths: Vec<String> = self
            .extras
            .iter()
            .map(|entry| entry.rel_path.clone())
            .collect();
        paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
        paths
    }
}

fn build_scan_report(
    source_id: &str,
    destination_id: &str,
    source_snapshot: &[SnapshotEntry],
    dst_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
    exact: bool,
) -> ScanReport {
    let diff = diff_manifests(source_snapshot, dst_snapshot, excludes, sync, exact);
    let mut report = ScanReport {
        source_id: source_id.to_string(),
        destination_id: destination_id.to_string(),
        scanned_at: Utc::now().to_rfc3339(),
        source_entries: source_snapshot.len() as u64,
        dst_entries: dst_snapshot.len() as u64,
        in_sync: diff.in_sync,
        ..Default::default()
    };
    let mut diffs: Vec<ScanDiffEntry> = Vec::new();
    // Keep a bounded sample PER KIND so each kind's "view files" popup has data,
    // regardless of how lopsided the totals are.
    let mut kind_pushed: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut push = |rel: &str, kind: &'static str, file_type: &str| {
        let count = kind_pushed.entry(kind).or_insert(0);
        if *count < SCAN_DIFF_PER_KIND_CAP {
            *count += 1;
            diffs.push(ScanDiffEntry {
                rel_path: rel.to_string(),
                kind: kind.to_string(),
                file_type: file_type.to_string(),
            });
        }
    };
    for (entry, existing) in &diff.transfer {
        if existing.is_none() {
            report.to_add += 1;
            push(&entry.rel_path, "add", &entry.file_type);
        } else {
            report.to_update += 1;
            push(&entry.rel_path, "update", &entry.file_type);
        }
    }
    for entry in &diff.missing_dirs {
        report.to_add += 1;
        push(&entry.rel_path, "add", &entry.file_type);
    }
    for entry in &diff.type_mismatch {
        report.type_mismatch += 1;
        push(&entry.rel_path, "type_mismatch", &entry.file_type);
    }
    for entry in &diff.mode_fixes {
        report.metadata += 1;
        push(&entry.rel_path, "metadata", &entry.file_type);
    }
    if sync.mirror {
        for entry in &diff.extras {
            report.to_delete += 1;
            push(&entry.rel_path, "delete", &entry.file_type);
        }
    }
    drop(push);
    diffs.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    let total = report.to_add
        + report.to_update
        + report.to_delete
        + report.type_mismatch
        + report.metadata;
    report.truncated = total > diffs.len() as u64;
    report.differences = diffs;
    report
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncRequestMode {
    #[default]
    Incremental,
    Full,
    /// Repair exactly the differences the last Compare reported (falls back
    /// to a full reconcile when the stored report is a truncated sample).
    RepairScan,
}

fn sync_request_mode_wire_value(mode: SyncRequestMode) -> &'static str {
    match mode {
        SyncRequestMode::Incremental => "incremental",
        SyncRequestMode::Full => "full",
        SyncRequestMode::RepairScan => "repair_scan",
    }
}

impl FromStr for SyncRequestMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "incremental" => Ok(Self::Incremental),
            "full" => Ok(Self::Full),
            "repair_scan" | "repair-scan" | "repair" => Ok(Self::RepairScan),
            other => bail!("unsupported sync mode: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferSnapshotMode {
    Source,
    Destination,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSnapshotRequest {
    pub root: PathBuf,
    pub mode: TransferSnapshotMode,
    #[serde(default)]
    pub excludes: Vec<PathBuf>,
    #[serde(default)]
    pub checksum: bool,
    /// What the requester is doing ("sync" or "compare"): the peer registers
    /// the served walk under this kind so a propagated cancel of a compare
    /// does not kill a sync's scan (and vice versa). Empty from old senders.
    #[serde(default)]
    pub purpose: String,
    /// The destination the requester's operation is scoped to
    /// ("source_id|destination_id", see [`crate::core::cancel::target_for`]):
    /// the peer registers the served walk under the same target so a
    /// destination-targeted cancel stops it too. Empty from old senders or
    /// multi-destination passes; such walks need an untargeted cancel.
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSnapshotPathsRequest {
    pub root: PathBuf,
    pub mode: TransferSnapshotMode,
    pub rel_paths: Vec<String>,
    /// Subset of `rel_paths` that must NOT be recursed when they are
    /// directories (`M`-only zfs-diff entries: the dir entry itself changed,
    /// every changed child has its own path). Absent/empty from old senders —
    /// everything then recurses, which is slower but always correct.
    #[serde(default)]
    pub non_recursive: Vec<String>,
    #[serde(default)]
    pub excludes: Vec<PathBuf>,
    #[serde(default)]
    pub checksum: bool,
    /// See [`TransferSnapshotRequest::purpose`].
    #[serde(default)]
    pub purpose: String,
    /// See [`TransferSnapshotRequest::scope`].
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPathInfoRequest {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPathInfo {
    pub kind: String,
    pub base: PathBuf,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPrepareDirRequest {
    pub root: PathBuf,
    pub rel_path: Option<String>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRemovePathRequest {
    pub root: PathBuf,
    pub rel_path: String,
    pub cycle_id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransferReceiveFileChunkQuery {
    pub root: String,
    pub rel_path: String,
    pub cycle_id: i64,
    pub size: i64,
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFileOffsetRequest {
    pub root: PathBuf,
    pub rel_path: String,
    pub cycle_id: i64,
    pub size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFileOffset {
    pub offset: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferFinishFileRequest {
    pub root: PathBuf,
    pub cycle_id: i64,
    pub entry: SnapshotEntry,
    /// blake3 of the whole source file, computed by the sender while streaming.
    /// The receiver re-hashes the assembled file and rejects a mismatch
    /// (end-to-end integrity). Optional for back-compat with older senders.
    #[serde(default)]
    pub full_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferReceiveSymlinkRequest {
    pub root: PathBuf,
    pub rel_path: String,
    pub cycle_id: i64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub hash: Option<String>,
    pub target: String,
    /// Raw target bytes from a Unix sender (targets are not required to be
    /// UTF-8 there); a Unix receiver prefers these over the lossy string.
    #[serde(default)]
    pub target_bytes: Option<Vec<u8>>,
    /// Whether the link points to a directory (decided by the sender). Needed so
    /// a Linux directory-symlink is recreated as a directory-symlink on Windows.
    #[serde(default)]
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPushFileRequest {
    pub source_root: PathBuf,
    pub rel_path: String,
    pub entry: SnapshotEntry,
    pub destination: crate::core::config::MachineConfig,
    pub destination_root: PathBuf,
    #[serde(default)]
    pub destination_id: String,
    pub cycle_id: i64,
    #[serde(default = "default_transfer_timeout_secs")]
    pub transfer_timeout_secs: u64,
    #[serde(default)]
    pub bwlimit_kbps: u64,
    /// The destination already holds a copy of this path, so an rsync-style
    /// delta against it may avoid re-sending unchanged regions.
    #[serde(default)]
    pub use_delta: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferAck {
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferDirSpec {
    pub rel_path: String,
    pub mode: u32,
    pub mtime_ns: i64,
}

/// Create many directories on the destination in a single request, eliminating
/// one HTTP round-trip per directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPrepareDirsRequest {
    pub root: PathBuf,
    pub dirs: Vec<TransferDirSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSetDirMtimesRequest {
    pub root: PathBuf,
    pub dirs: Vec<TransferDirSpec>,
}

/// Chmod content-equal destination files whose permission bits drifted, in
/// one request — a mode-only difference must not re-transfer file content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSetModesRequest {
    pub root: PathBuf,
    pub items: Vec<TransferModeSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferModeSpec {
    pub rel_path: String,
    pub mode: u32,
}

/// Remove many destination paths in a single request. Paths are removed in the
/// order given (callers pass deepest-first so directories empty before removal).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRemovePathsRequest {
    pub root: PathBuf,
    pub rel_paths: Vec<String>,
    pub cycle_id: i64,
}

/// Remove the destination's per-cycle temp directory once the cycle's transfers
/// are complete (replaces the previous per-file cleanup, which is unsafe under
/// parallel transfers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferCleanupTmpRequest {
    pub root: PathBuf,
    pub cycle_id: i64,
    /// Trash retention window in days (None = receiver default of 30).
    #[serde(default)]
    pub trash_keep_days: Option<u64>,
}

/// Request the destination's per-block checksums for an existing file so the
/// source can compute a delta. Returns empty blocks when the file is absent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferBlockSumsRequest {
    pub root: PathBuf,
    pub rel_path: String,
}

/// Query for applying a delta. The encoded delta is the request body; the old
/// file already on the destination supplies the copied regions.
#[derive(Debug, Clone, Deserialize)]
pub struct TransferApplyDeltaQuery {
    pub root: String,
    pub rel_path: String,
    pub cycle_id: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub mode: u32,
    pub full_hash: String,
}

/// Query for the single-round-trip small-file fast path. The file bytes are the
/// request body; metadata travels in the query string.
#[derive(Debug, Clone, Deserialize)]
pub struct TransferPutFileQuery {
    pub root: String,
    pub rel_path: String,
    pub cycle_id: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub mode: u32,
    /// blake3 of the file body for end-to-end integrity (see
    /// [`TransferFinishFileRequest::full_hash`]). Optional for back-compat.
    #[serde(default)]
    pub full_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPutFilesBatchQuery {
    pub root: String,
    pub cycle_id: i64,
}

/// One frame header in a put-files-batch body: the JSON line is followed by
/// exactly `size` raw payload bytes, then the next frame (or end of body).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchFileHeader {
    rel_path: String,
    size: i64,
    mtime_ns: i64,
    mode: u32,
    /// blake3 of the payload; mandatory on this path (end-to-end integrity).
    full_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchFileFailure {
    pub rel_path: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferBatchAck {
    pub ok: bool,
    #[serde(default)]
    pub failed: Vec<BatchFileFailure>,
}

/// Delegated batch push: the controller asks the SOURCE machine to read these
/// small files and deliver them to the destination as one batched request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPushFilesBatchRequest {
    pub source_root: PathBuf,
    pub entries: Vec<SnapshotEntry>,
    pub destination: crate::core::config::MachineConfig,
    pub destination_root: PathBuf,
    pub destination_id: String,
    pub cycle_id: i64,
    pub transfer_timeout_secs: u64,
    pub bwlimit_kbps: u64,
}

/// Per-file result of a batch push: how many actually landed, which sources
/// were caught changing mid-read (tolerated), and which failed for per-file
/// reasons on either side.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferBatchOutcome {
    pub sent: u64,
    #[serde(default)]
    pub changing: Vec<String>,
    #[serde(default)]
    pub failed: Vec<BatchFileFailure>,
}

#[derive(Debug, Clone, Deserialize)]
struct PeerRuntimeStatus {
    scan: Option<PeerScanProgress>,
}

#[derive(Debug, Clone, Deserialize)]
struct PeerScanProgress {
    root_path: String,
    current_path: String,
    entries_seen: u64,
}

fn default_transfer_timeout_secs() -> u64 {
    DEFAULT_TRANSFER_TIMEOUT_SECS
}

fn transfer_timeout(sync: &NativeSyncConfig) -> Duration {
    Duration::from_secs(sync.transfer_timeout_secs.max(1))
}

/// Minimum timeout for whole-tree snapshot requests. Unlike per-file transfer
/// calls, the response only starts after the peer finishes walking the entire
/// tree (and re-hashing every file in checksum mode), so the configured
/// transfer timeout — sized for one file — would kill any large-tree snapshot.
/// A genuinely hung peer still fails, just later.
fn snapshot_timeout_floor(checksum: bool) -> Duration {
    if checksum {
        // Full-content re-hash of the tree: hours on multi-terabyte trees.
        Duration::from_secs(6 * 3600)
    } else {
        Duration::from_secs(3600)
    }
}

fn snapshot_timeout(sync: &NativeSyncConfig) -> Duration {
    transfer_timeout(sync).max(snapshot_timeout_floor(sync.checksum))
}

fn effective_sync_config(cfg: &AppConfig, dst: &DestinationConfig) -> NativeSyncConfig {
    dst.sync.clone().unwrap_or_else(|| cfg.app.sync.clone())
}

fn any_ready_destination_needs_checksum(
    cfg: &AppConfig,
    source: &SourceGroupConfig,
    ready_destinations: &[usize],
) -> bool {
    ready_destinations
        .iter()
        .any(|index| effective_sync_config(cfg, &source.destinations[*index]).checksum)
}

fn ready_destination_timeout(
    cfg: &AppConfig,
    source: &SourceGroupConfig,
    ready_destinations: &[usize],
) -> Duration {
    ready_destinations
        .iter()
        .map(|index| transfer_timeout(&effective_sync_config(cfg, &source.destinations[*index])))
        .max()
        .unwrap_or_else(|| transfer_timeout(&cfg.app.sync))
}

/// The cancel kind for a snapshot request: what the requester declared, or
/// "sync" for old senders that carry no purpose. Registering here makes a
/// peer-served walk individually cancellable — a propagated cancel reaches
/// the machine actually burning disk time.
fn snapshot_request_cancel_kind(purpose: &str) -> &str {
    if purpose == cancel::KIND_COMPARE {
        cancel::KIND_COMPARE
    } else {
        cancel::KIND_SYNC
    }
}

/// The destination target a peer-served walk registers under: the requester's
/// declared scope, or none for old senders (cancellable only untargeted).
fn snapshot_request_cancel_target(scope: &str) -> Option<String> {
    let scope = scope.trim();
    (!scope.is_empty()).then(|| scope.to_string())
}

pub fn transfer_snapshot(req: TransferSnapshotRequest) -> Result<Vec<SnapshotEntry>> {
    let _cancellable = cancel::begin_target(
        snapshot_request_cancel_kind(&req.purpose),
        snapshot_request_cancel_target(&req.scope),
    );
    match req.mode {
        TransferSnapshotMode::Source => take_snapshot_with_excludes(
            &req.root,
            SnapshotMode::Source,
            &req.excludes,
            req.checksum,
        ),
        TransferSnapshotMode::Destination => {
            reject_dangerous_destination(&req.root)?;
            if !req.root.exists() {
                return Ok(Vec::new());
            }
            take_snapshot_with_excludes(&req.root, SnapshotMode::Destination, &[], req.checksum)
        }
    }
}

/// Terminal line of a streamed snapshot response: the walk's outcome arrives
/// AFTER the entries (a 200 status was already sent when streaming began), so
/// the requester must see an explicit ok before trusting the manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotStreamStatus {
    #[serde(rename = "__status__")]
    pub status: String,
    #[serde(default)]
    pub message: String,
}

impl SnapshotStreamStatus {
    pub fn from_result(result: &Result<()>) -> Self {
        match result {
            Ok(()) => Self {
                status: "ok".to_string(),
                message: String::new(),
            },
            Err(err) => Self {
                status: "error".to_string(),
                message: format!("{err:#}"),
            },
        }
    }
}

/// Target size of one streamed NDJSON buffer handed to the HTTP layer.
const SNAPSHOT_STREAM_BUFFER: usize = 48 * 1024;

/// [`transfer_snapshot`] in streaming form: entries are serialized and handed
/// to `sink` (one NDJSON line each, batched into ~48KB buffers) while the
/// walk runs, so the serving peer never buffers the whole manifest or its
/// JSON text. The caller appends the [`SnapshotStreamStatus`] line.
pub fn transfer_snapshot_stream(
    req: TransferSnapshotRequest,
    sink: &mut dyn FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
    let _cancellable = cancel::begin_target(
        snapshot_request_cancel_kind(&req.purpose),
        snapshot_request_cancel_target(&req.scope),
    );
    let mut buf: Vec<u8> = Vec::with_capacity(SNAPSHOT_STREAM_BUFFER + 1024);
    {
        let mut emit = |entry: SnapshotEntry| -> Result<()> {
            serde_json::to_writer(&mut buf, &entry)
                .context("failed to serialize snapshot entry")?;
            buf.push(b'\n');
            if buf.len() >= SNAPSHOT_STREAM_BUFFER {
                sink(std::mem::take(&mut buf))?;
            }
            Ok(())
        };
        match req.mode {
            TransferSnapshotMode::Source => take_snapshot_with_excludes_streamed(
                &req.root,
                SnapshotMode::Source,
                &req.excludes,
                req.checksum,
                &mut emit,
            )?,
            TransferSnapshotMode::Destination => {
                reject_dangerous_destination(&req.root)?;
                if req.root.exists() {
                    take_snapshot_with_excludes_streamed(
                        &req.root,
                        SnapshotMode::Destination,
                        &[],
                        req.checksum,
                        &mut emit,
                    )?;
                }
            }
        }
    }
    if !buf.is_empty() {
        sink(buf)?;
    }
    Ok(())
}

pub fn transfer_snapshot_paths(req: TransferSnapshotPathsRequest) -> Result<Vec<SnapshotEntry>> {
    let _cancellable = cancel::begin_target(
        snapshot_request_cancel_kind(&req.purpose),
        snapshot_request_cancel_target(&req.scope),
    );
    let non_recursive: BTreeSet<&str> = req.non_recursive.iter().map(String::as_str).collect();
    let paths: Vec<DiffPath> = req
        .rel_paths
        .iter()
        .map(|rel| DiffPath {
            rel: rel.clone(),
            recursive: !non_recursive.contains(rel.as_str()),
        })
        .collect();
    match req.mode {
        TransferSnapshotMode::Source => take_snapshot_diff_paths_with_excludes(
            &req.root,
            &paths,
            SnapshotMode::Source,
            &req.excludes,
            req.checksum,
        ),
        TransferSnapshotMode::Destination => {
            reject_dangerous_destination(&req.root)?;
            if !req.root.exists() {
                return Ok(Vec::new());
            }
            take_snapshot_diff_paths_with_excludes(
                &req.root,
                &paths,
                SnapshotMode::Destination,
                &[],
                req.checksum,
            )
        }
    }
}

pub fn transfer_path_info(req: TransferPathInfoRequest) -> Result<TransferPathInfo> {
    let metadata = fs::symlink_metadata(&req.path)
        .with_context(|| format!("failed to read path {}", req.path.display()))?;
    let name = cross_platform_file_name(&req.path)
        .ok_or_else(|| anyhow!("path has no file name: {}", req.path.display()))?;
    if metadata.is_dir() {
        return Ok(TransferPathInfo {
            kind: "dir".to_string(),
            base: req.path,
            name,
        });
    }
    if metadata.is_file() || metadata.file_type().is_symlink() {
        let base = req
            .path
            .parent()
            .ok_or_else(|| anyhow!("file path has no parent: {}", req.path.display()))?
            .to_path_buf();
        return Ok(TransferPathInfo {
            kind: "file".to_string(),
            base,
            name,
        });
    }
    bail!(
        "path is neither a file nor a directory: {}",
        req.path.display()
    )
}

pub fn transfer_prepare_dir(req: TransferPrepareDirRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    let path = match req.rel_path.as_deref() {
        Some(rel_path) => safe_join_rel(&req.root, rel_path)?,
        None => req.root,
    };
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create directory {}", path.display()))?;
    // Mode is applied later via set-dir-mtimes (deepest-first) so a read-only
    // directory does not block writing its children during transfer.
    let _ = req.mode;
    Ok(transfer_ack())
}

pub fn transfer_remove_path(req: TransferRemovePathRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    let path = safe_join_rel(&req.root, &req.rel_path)?;
    if !path.exists() && fs::symlink_metadata(&path).is_err() {
        return Ok(transfer_ack());
    }
    move_to_trash(&req.root, &req.rel_path, req.cycle_id)?;
    Ok(transfer_ack())
}

pub fn transfer_cleanup_tmp(req: TransferCleanupTmpRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    cleanup_tmp_cycle(&req.root, req.cycle_id);
    cleanup_expired_trash(&req.root, req.trash_keep_days.unwrap_or(30));
    Ok(transfer_ack())
}

pub fn transfer_prepare_dirs(req: TransferPrepareDirsRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    for dir in &req.dirs {
        let path = if dir.rel_path.is_empty() {
            req.root.clone()
        } else {
            safe_join_rel(&req.root, &dir.rel_path)?
        };
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create directory {}", path.display()))?;
        // Mode applied later via set-dir-mtimes (deepest-first).
    }
    Ok(transfer_ack())
}

pub fn transfer_set_dir_mtimes(req: TransferSetDirMtimesRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    set_dir_mtimes(&req.root, &req.dirs)?;
    Ok(transfer_ack())
}

pub fn transfer_set_modes(req: TransferSetModesRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    for item in &req.items {
        let path = safe_join_rel(&req.root, &item.rel_path)?;
        // Vanished since the comparison: nothing left to repair.
        if fs::symlink_metadata(&path).is_err() {
            continue;
        }
        set_mode(&path, item.mode)
            .with_context(|| format!("failed to set mode on {}", item.rel_path))?;
    }
    Ok(transfer_ack())
}

pub fn transfer_remove_paths(req: TransferRemovePathsRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    for rel_path in &req.rel_paths {
        let path = safe_join_rel(&req.root, rel_path)?;
        if !path.exists() && fs::symlink_metadata(&path).is_err() {
            continue;
        }
        move_to_trash(&req.root, rel_path, req.cycle_id)
            .with_context(|| format!("failed to remove destination path {rel_path}"))?;
    }
    Ok(transfer_ack())
}

pub fn transfer_file_offset(req: TransferFileOffsetRequest) -> Result<TransferFileOffset> {
    reject_dangerous_destination(&req.root)?;
    let tmp = tmp_path(&req.root, req.cycle_id, &req.rel_path);
    let offset = match fs::metadata(&tmp) {
        Ok(metadata) if metadata.len() <= req.size.max(0) as u64 => metadata.len(),
        Ok(_) => {
            remove_any(&tmp).ok();
            0
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => 0,
        Err(err) => {
            return Err(err).with_context(|| format!("failed to inspect {}", tmp.display()));
        }
    };
    Ok(TransferFileOffset { offset })
}

pub fn transfer_receive_file_chunk(
    query: TransferReceiveFileChunkQuery,
    bytes: &[u8],
) -> Result<TransferAck> {
    let root = Path::new(&query.root);
    reject_dangerous_destination(root)?;
    let size = query.size.max(0) as u64;
    let end = query
        .offset
        .checked_add(bytes.len() as u64)
        .ok_or_else(|| anyhow!("file chunk offset overflow"))?;
    if end > size {
        bail!(
            "received file chunk exceeds expected size for {}",
            query.rel_path
        );
    }
    let tmp = tmp_path(root, query.cycle_id, &query.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if query.offset == 0 && (tmp.exists() || fs::symlink_metadata(&tmp).is_ok()) {
        remove_any(&tmp)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&tmp)
        .with_context(|| format!("failed to open temp file {}", tmp.display()))?;
    let current_len = file.metadata()?.len();
    if current_len < query.offset {
        bail!(
            "resume offset {} is beyond temp file length {} for {}",
            query.offset,
            current_len,
            query.rel_path
        );
    }
    if current_len > query.offset {
        file.set_len(query.offset)?;
    }
    file.seek(SeekFrom::Start(query.offset))?;
    file.write_all(bytes)?;
    Ok(transfer_ack())
}

pub fn transfer_finish_file(req: TransferFinishFileRequest) -> Result<TransferAck> {
    reject_dangerous_destination(&req.root)?;
    if req.entry.file_type != "file" {
        bail!("transfer_finish_file requires a file entry");
    }
    let final_path = safe_join_rel(&req.root, &req.entry.rel_path)?;
    let tmp = tmp_path(&req.root, req.cycle_id, &req.entry.rel_path);
    let len = fs::metadata(&tmp)
        .with_context(|| format!("missing temp file {}", tmp.display()))?
        .len();
    if len != req.entry.size.max(0) as u64 {
        bail!(
            "received file size mismatch for {}: got {}, expected {}",
            req.entry.rel_path,
            len,
            req.entry.size
        );
    }
    // Carry the streamed full-file hash into the entry so finish_received_file
    // verifies the assembled chunks end-to-end before publishing.
    let mut entry = req.entry;
    if entry.hash.is_none() {
        entry.hash = req.full_hash;
    }
    finish_received_file(&req.root, req.cycle_id, &entry, &tmp, &final_path)?;
    Ok(transfer_ack())
}

/// Single-round-trip small-file write: the whole file body is delivered in one
/// request and finished immediately (no separate offset/chunk/finish calls).
pub fn transfer_put_file(query: TransferPutFileQuery, bytes: &[u8]) -> Result<TransferAck> {
    let root = PathBuf::from(&query.root);
    reject_dangerous_destination(&root)?;
    let size = query.size.max(0) as u64;
    if bytes.len() as u64 != size {
        bail!(
            "put-file size mismatch for {}: got {}, expected {}",
            query.rel_path,
            bytes.len(),
            size
        );
    }
    // Verify content end-to-end against the sender's hash before writing, so a
    // bit flipped in transit (which TCP's checksum can miss) is rejected.
    if let Some(expected) = &query.full_hash {
        let actual = blake3::hash(bytes).to_hex().to_string();
        if &actual != expected {
            bail!("put-file content hash mismatch for {}", query.rel_path);
        }
    }
    let entry = SnapshotEntry {
        rel_path: query.rel_path.clone(),
        file_type: "file".to_string(),
        size: query.size,
        mtime_ns: query.mtime_ns,
        mode: query.mode,
        hash: query.full_hash.clone(),
    };
    let final_path = safe_join_rel(&root, &entry.rel_path)?;
    let tmp = tmp_path(&root, query.cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&tmp, bytes)
        .with_context(|| format!("failed to write temp file {}", tmp.display()))?;
    finish_received_file(&root, query.cycle_id, &entry, &tmp, &final_path)?;
    Ok(transfer_ack())
}

/// Many small files in ONE request with ONE durability barrier. The body is a
/// sequence of frames — `<json BatchFileHeader>\n<raw payload bytes>` — and
/// the receiver stages every file first, fsyncs them as a group (consecutive
/// fsyncs after all writes coalesce into roughly one log flush on ZFS, versus
/// one synchronous flush per file on the per-file endpoint), then publishes
/// the renames. Per-file failures are reported in the ack and do not abort
/// the rest of the batch.
pub fn transfer_put_files_batch(
    query: TransferPutFilesBatchQuery,
    body: &[u8],
) -> Result<TransferBatchAck> {
    let root = PathBuf::from(&query.root);
    reject_dangerous_destination(&root)?;
    let mut failed: Vec<BatchFileFailure> = Vec::new();
    let mut staged: Vec<(SnapshotEntry, PathBuf, PathBuf)> = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        // Frame boundaries come from the header's size field; a malformed
        // frame poisons everything after it, so that IS a whole-request error.
        let newline = rest
            .iter()
            .position(|byte| *byte == b'\n')
            .ok_or_else(|| anyhow!("malformed batch body: missing header terminator"))?;
        let header: BatchFileHeader =
            serde_json::from_slice(&rest[..newline]).context("malformed batch file header")?;
        rest = &rest[newline + 1..];
        let size = usize::try_from(header.size.max(0))
            .map_err(|_| anyhow!("batch file size overflow for {}", header.rel_path))?;
        if rest.len() < size {
            bail!(
                "malformed batch body: truncated payload for {}",
                header.rel_path
            );
        }
        let (payload, tail) = rest.split_at(size);
        rest = tail;
        let rel_path = header.rel_path.clone();
        let stage = (|| -> Result<()> {
            // Verify content end-to-end BEFORE writing (bit flips in transit
            // that TCP's checksum can miss).
            let actual = blake3::hash(payload).to_hex().to_string();
            if actual != header.full_hash {
                bail!("batch file content hash mismatch for {}", header.rel_path);
            }
            let entry = SnapshotEntry {
                rel_path: header.rel_path.clone(),
                file_type: "file".to_string(),
                size: header.size,
                mtime_ns: header.mtime_ns,
                mode: header.mode,
                hash: Some(header.full_hash.clone()),
            };
            let final_path = safe_join_rel(&root, &entry.rel_path)?;
            let tmp = tmp_path(&root, query.cycle_id, &entry.rel_path);
            if let Some(parent) = tmp.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&tmp, payload)
                .with_context(|| format!("failed to write temp file {}", tmp.display()))?;
            staged.push((entry, tmp, final_path));
            Ok(())
        })();
        if let Err(err) = stage {
            failed.push(BatchFileFailure {
                rel_path,
                error: format!("{err:#}"),
            });
        }
    }

    // Durability barrier for the whole batch: verify + set metadata + fsync
    // every staged file BEFORE any rename publishes it (same per-file order
    // as finish_received_file, batched).
    let mut publish: Vec<(SnapshotEntry, PathBuf, PathBuf)> = Vec::new();
    for (entry, tmp, final_path) in staged {
        let step = (|| -> Result<()> {
            let actual = hash_file(&tmp)?;
            if entry.hash.as_deref() != Some(actual.as_str()) {
                remove_any(&tmp).ok();
                bail!("received file hash mismatch at {}", entry.rel_path);
            }
            fsync_file(&tmp)
                .with_context(|| format!("failed to fsync received file {}", entry.rel_path))?;
            set_mode(&tmp, entry.mode).ok();
            let mtime = FileTime::from_unix_time(
                entry.mtime_ns / 1_000_000_000,
                (entry.mtime_ns % 1_000_000_000) as u32,
            );
            if let Err(err) = set_file_mtime(&tmp, mtime) {
                warn!(rel_path = entry.rel_path, error = %err, "failed to set received file mtime");
            }
            Ok(())
        })();
        match step {
            Ok(()) => publish.push((entry, tmp, final_path)),
            Err(err) => failed.push(BatchFileFailure {
                rel_path: entry.rel_path.clone(),
                error: format!("{err:#}"),
            }),
        }
    }
    // Publish the batch and flush each touched parent directory once.
    let mut parents: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for (entry, tmp, final_path) in publish {
        let step = (|| -> Result<()> {
            if let Some(parent) = final_path.parent() {
                fs::create_dir_all(parent)?;
            }
            replace_path(&root, query.cycle_id, &entry.rel_path, &tmp, &final_path)?;
            Ok(())
        })();
        match step {
            Ok(()) => {
                if let Some(parent) = final_path.parent() {
                    parents
                        .entry(parent.to_path_buf())
                        .or_insert_with(|| final_path.clone());
                }
            }
            Err(err) => failed.push(BatchFileFailure {
                rel_path: entry.rel_path.clone(),
                error: format!("{err:#}"),
            }),
        }
    }
    for sample_child in parents.values() {
        fsync_parent(sample_child).ok();
    }
    Ok(TransferBatchAck { ok: true, failed })
}

/// Open (and position) the tmp file a streamed big-file body will be written
/// into, with the same validation and resume semantics as the 16 MiB chunk
/// endpoint. Split out so the async streaming handler can run it on the
/// blocking pool before consuming the body.
pub fn transfer_open_file_stream_target(query: &TransferReceiveFileChunkQuery) -> Result<File> {
    let root = Path::new(&query.root);
    reject_dangerous_destination(root)?;
    let size = query.size.max(0) as u64;
    if query.offset > size {
        bail!(
            "stream offset {} exceeds expected size {} for {}",
            query.offset,
            size,
            query.rel_path
        );
    }
    let tmp = tmp_path(root, query.cycle_id, &query.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if query.offset == 0 && (tmp.exists() || fs::symlink_metadata(&tmp).is_ok()) {
        remove_any(&tmp)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&tmp)
        .with_context(|| format!("failed to open temp file {}", tmp.display()))?;
    let current_len = file.metadata()?.len();
    if current_len < query.offset {
        bail!(
            "resume offset {} is beyond temp file length {} for {}",
            query.offset,
            current_len,
            query.rel_path
        );
    }
    if current_len > query.offset {
        file.set_len(query.offset)?;
    }
    file.seek(SeekFrom::Start(query.offset))?;
    Ok(file)
}

pub fn transfer_block_sums(req: TransferBlockSumsRequest) -> Result<delta::BlockSums> {
    reject_dangerous_destination(&req.root)?;
    let path = safe_join_rel(&req.root, &req.rel_path)?;
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => {
            return Ok(delta::BlockSums {
                block_len: 0,
                file_size: 0,
                blocks: Vec::new(),
            });
        }
    };
    let block_len = delta::block_len_for(metadata.len());
    let file = File::open(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let blocks = delta::compute_block_sums_from_reader(io::BufReader::new(file), block_len)
        .with_context(|| format!("failed to checksum {}", path.display()))?;
    Ok(delta::BlockSums {
        block_len: block_len as u32,
        file_size: metadata.len(),
        blocks,
    })
}

pub fn transfer_apply_delta(
    query: TransferApplyDeltaQuery,
    delta_bytes: &[u8],
) -> Result<TransferAck> {
    let root = PathBuf::from(&query.root);
    reject_dangerous_destination(&root)?;
    let final_path = safe_join_rel(&root, &query.rel_path)?;
    let old_path = final_path.clone();
    let tmp = tmp_path(&root, query.cycle_id, &query.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    {
        let mut old = File::open(&old_path)
            .with_context(|| format!("missing delta basis file {}", old_path.display()))?;
        let mut out = File::create(&tmp)
            .with_context(|| format!("failed to create temp file {}", tmp.display()))?;
        delta::apply_delta(&mut old, delta_bytes, &mut out)?;
        out.flush()?;
    }
    let len = fs::metadata(&tmp)?.len();
    if len != query.size.max(0) as u64 {
        remove_any(&tmp).ok();
        bail!(
            "delta result size mismatch for {}: got {}, expected {}",
            query.rel_path,
            len,
            query.size
        );
    }
    let entry = SnapshotEntry {
        rel_path: query.rel_path.clone(),
        file_type: "file".to_string(),
        size: query.size,
        mtime_ns: query.mtime_ns,
        mode: query.mode,
        hash: Some(query.full_hash.clone()),
    };
    finish_received_file(&root, query.cycle_id, &entry, &tmp, &final_path)?;
    Ok(transfer_ack())
}

pub fn transfer_receive_symlink(req: TransferReceiveSymlinkRequest) -> Result<TransferAck> {
    let entry = SnapshotEntry {
        rel_path: req.rel_path,
        file_type: "symlink".to_string(),
        size: 0,
        mtime_ns: req.mtime_ns,
        mode: req.mode,
        hash: req.hash,
    };
    // Unix receivers prefer the sender's raw bytes (byte-faithful for
    // non-UTF-8 targets); everyone else uses the wire string, converting the
    // separators to the local convention so relative targets still resolve.
    #[cfg(unix)]
    let target: PathBuf = match req.target_bytes {
        Some(bytes) => {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        }
        None => PathBuf::from(&req.target),
    };
    #[cfg(not(unix))]
    let target: PathBuf = PathBuf::from(req.target.replace('/', "\\"));
    receive_symlink_target(&req.root, req.cycle_id, &entry, &target, req.is_dir)?;
    Ok(transfer_ack())
}

pub fn transfer_push_file(req: TransferPushFileRequest) -> Result<TransferAck> {
    let src = safe_join_rel(&req.source_root, &req.rel_path)?;
    let timeout = Duration::from_secs(req.transfer_timeout_secs.max(1));
    match req.entry.file_type.as_str() {
        "file" => {
            let size = req.entry.size.max(0) as u64;
            let use_delta = req.use_delta
                && req.entry.hash.is_none()
                && (DELTA_MIN_SIZE..=DELTA_MAX_SIZE).contains(&size);
            if use_delta {
                send_file_delta(
                    &req.destination,
                    &req.destination_root,
                    &req.destination_id,
                    req.cycle_id,
                    &req.entry,
                    &src,
                    timeout,
                    req.bwlimit_kbps,
                )?;
            } else {
                send_file_tcp(
                    &req.destination,
                    &req.destination_root,
                    &req.destination_id,
                    req.cycle_id,
                    &req.entry,
                    &src,
                    timeout,
                    req.bwlimit_kbps,
                )?;
            }
        }
        "symlink" => {
            send_symlink_tcp(
                &req.destination,
                &req.destination_root,
                req.cycle_id,
                &req.entry,
                &src,
                Duration::from_secs(req.transfer_timeout_secs.max(1)),
            )?;
        }
        other => bail!("unsupported transfer entry type {other}"),
    }
    Ok(transfer_ack())
}

fn transfer_ack() -> TransferAck {
    TransferAck { ok: true }
}

/// Upper bound for a file to ride the small-file batch path.
const SMALL_BATCH_FILE_MAX: u64 = 256 * 1024;
/// Caps for one batch request: bounded sender/receiver memory and bounded
/// retry cost while still amortizing the per-request round-trip and the
/// receiver's durability barrier over many files.
const SMALL_BATCH_MAX_BYTES: usize = 8 * 1024 * 1024;
const SMALL_BATCH_MAX_FILES: usize = 200;

/// Read the batch's files, verify them against their snapshot metadata, and
/// deliver them to the destination as ONE put-files-batch request (falling
/// back to per-file sends for peers without the endpoint). Sources caught
/// changing mid-read are classified per file, not failed.
pub fn transfer_push_files_batch(
    req: TransferPushFilesBatchRequest,
) -> Result<TransferBatchOutcome> {
    let timeout = Duration::from_secs(req.transfer_timeout_secs.max(1));
    let mut outcome = TransferBatchOutcome::default();
    let mut body: Vec<u8> = Vec::new();
    let mut included: Vec<&SnapshotEntry> = Vec::new();
    let mut payload_bytes = 0_usize;
    for entry in &req.entries {
        cancel::check()?;
        let read = (|| -> Result<Vec<u8>> {
            let src = safe_join_rel(&req.source_root, &entry.rel_path)?;
            let bytes =
                fs::read(&src).with_context(|| format!("failed to read {}", src.display()))?;
            if bytes.len() as u64 != entry.size.max(0) as u64 {
                bail!("source changed while copying {}", entry.rel_path);
            }
            if let Some(expected) = &entry.hash {
                if blake3::hash(&bytes).to_hex().to_string() != *expected {
                    bail!("source changed while copying {}", entry.rel_path);
                }
            }
            // Catch same-size torn reads (mutation mid-read keeps the length).
            ensure_source_stable(&src, entry)?;
            Ok(bytes)
        })();
        match read {
            Ok(bytes) => {
                let header = BatchFileHeader {
                    rel_path: entry.rel_path.clone(),
                    size: entry.size,
                    mtime_ns: entry.mtime_ns,
                    mode: entry.mode,
                    full_hash: blake3::hash(&bytes).to_hex().to_string(),
                };
                body.extend_from_slice(&serde_json::to_vec(&header)?);
                body.push(b'\n');
                payload_bytes += bytes.len();
                body.extend_from_slice(&bytes);
                included.push(entry);
            }
            Err(err) if transfer_error_is_source_changing(&err) => {
                outcome.changing.push(entry.rel_path.clone());
            }
            Err(err) => outcome.failed.push(BatchFileFailure {
                rel_path: entry.rel_path.clone(),
                error: format!("{err:#}"),
            }),
        }
    }
    if included.is_empty() {
        return Ok(outcome);
    }
    let api_path = format!(
        "/api/transfer/put-files-batch?root={}&cycle_id={}",
        encode_query_component(&req.destination_root.to_string_lossy()),
        req.cycle_id
    );
    let ack: TransferBatchAck = match remote_post_bytes(&req.destination, &api_path, &body, timeout)
    {
        Ok(ack) => ack,
        Err(err) if error_is_missing_endpoint(&err) => {
            // Old peer: deliver the batch per file through the classic
            // endpoints (send_file_tcp routes small files to put-file).
            return push_batch_per_file_fallback(&req, &included, outcome, timeout);
        }
        Err(err) => return Err(err),
    };
    if !ack.ok {
        bail!("peer rejected put-files-batch request");
    }
    let rejected: BTreeSet<&str> = ack
        .failed
        .iter()
        .map(|failure| failure.rel_path.as_str())
        .collect();
    for entry in &included {
        if rejected.contains(entry.rel_path.as_str()) {
            continue;
        }
        outcome.sent += 1;
        progress::record_transfer(&entry.rel_path, entry.size.max(0) as u64);
    }
    outcome.failed.extend(ack.failed);
    throttle_after_transfer(payload_bytes, req.bwlimit_kbps);
    Ok(outcome)
}

/// Per-file delivery of a planned batch for peers without the batch endpoint.
fn push_batch_per_file_fallback(
    req: &TransferPushFilesBatchRequest,
    included: &[&SnapshotEntry],
    mut outcome: TransferBatchOutcome,
    timeout: Duration,
) -> Result<TransferBatchOutcome> {
    for entry in included {
        cancel::check()?;
        let src = safe_join_rel(&req.source_root, &entry.rel_path)?;
        match send_file_tcp(
            &req.destination,
            &req.destination_root,
            &req.destination_id,
            req.cycle_id,
            entry,
            &src,
            timeout,
            req.bwlimit_kbps,
        ) {
            Ok(()) => outcome.sent += 1,
            Err(err) if transfer_error_is_source_changing(&err) => {
                outcome.changing.push(entry.rel_path.clone());
            }
            Err(err) => outcome.failed.push(BatchFileFailure {
                rel_path: entry.rel_path.clone(),
                error: format!("{err:#}"),
            }),
        }
    }
    Ok(outcome)
}

fn receive_symlink_target(
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    target: &Path,
    is_dir: bool,
) -> Result<()> {
    reject_dangerous_destination(dst_root)?;
    if entry.file_type != "symlink" {
        bail!("receive_symlink_target requires a symlink entry");
    }
    if target.is_absolute() {
        // Copied verbatim for backup fidelity, but it points wherever the
        // DESTINATION machine has that path — usually nowhere.
        warn!(
            rel_path = entry.rel_path,
            target = %target.display(),
            "symlink has an absolute target; it will not resolve portably on this machine"
        );
    }
    let final_path = safe_join_rel(dst_root, &entry.rel_path)?;
    let tmp = tmp_path(dst_root, cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    if tmp.exists() || fs::symlink_metadata(&tmp).is_ok() {
        remove_any(&tmp)?;
    }
    create_symlink_kind(target, &tmp, is_dir)
        .with_context(|| format!("failed to create symlink {}", tmp.display()))?;
    if Some(hash_symlink(&tmp)?) != entry.hash {
        remove_any(&tmp).ok();
        bail!("received symlink hash mismatch at {}", entry.rel_path);
    }
    replace_path(dst_root, cycle_id, &entry.rel_path, &tmp, &final_path)?;
    fsync_parent(&final_path).ok();
    Ok(())
}

fn finish_received_file(
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    tmp: &Path,
    final_path: &Path,
) -> Result<()> {
    if let Some(expected_hash) = &entry.hash {
        let actual_hash = hash_file(tmp)?;
        if &actual_hash != expected_hash {
            remove_any(tmp).ok();
            bail!("received file hash mismatch at {}", entry.rel_path);
        }
    }
    // Flush data before tightening mode and renaming: a swallowed fsync error
    // here could publish a zero-length/stale file as "verified" after a crash.
    fsync_file(tmp).with_context(|| format!("failed to fsync received file {}", entry.rel_path))?;
    set_mode(tmp, entry.mode).ok();
    let mtime = FileTime::from_unix_time(
        entry.mtime_ns / 1_000_000_000,
        (entry.mtime_ns % 1_000_000_000) as u32,
    );
    if let Err(err) = set_file_mtime(tmp, mtime) {
        // A file whose mtime cannot be recorded will compare as changed and
        // re-transfer every cycle; make that visible instead of silent.
        warn!(rel_path = entry.rel_path, error = %err, "failed to set received file mtime");
    }
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)?;
    }
    replace_path(dst_root, cycle_id, &entry.rel_path, tmp, final_path)?;
    fsync_parent(final_path).ok();
    Ok(())
}

fn send_file_tcp(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    timeout: Duration,
    bwlimit_kbps: u64,
) -> Result<()> {
    let total_size = entry.size.max(0) as u64;
    // Small-file fast path: deliver the whole file in a single round-trip
    // instead of file-offset + chunk + finish. Skipped when a checksum hash is
    // present (the chunked path verifies it) so behaviour is unchanged.
    if total_size <= TRANSFER_CHUNK_SIZE as u64 && entry.hash.is_none() {
        return send_put_file_tcp(
            destination,
            destination_root,
            destination_id,
            cycle_id,
            entry,
            src,
            timeout,
            bwlimit_kbps,
        );
    }
    let offset_req = TransferFileOffsetRequest {
        root: destination_root.to_path_buf(),
        rel_path: entry.rel_path.clone(),
        cycle_id,
        size: entry.size,
    };
    let offset_response: TransferFileOffset = remote_post_json(
        destination,
        "/api/transfer/file-offset",
        &offset_req,
        timeout,
    )?;
    let mut offset = offset_response.offset;
    if offset > total_size {
        offset = 0;
    }
    let _ = destination_id;
    // One streamed request delivers the whole remainder (the receiver writes
    // it straight to disk); peers without the endpoint get the legacy 16 MiB
    // request-per-chunk loop.
    let full_hash = match send_file_body_streamed(
        destination,
        destination_root,
        cycle_id,
        entry,
        src,
        offset,
        timeout,
        bwlimit_kbps,
    ) {
        Ok(hash) => hash,
        Err(err) if error_is_missing_endpoint(&err) => send_file_body_chunked(
            destination,
            destination_root,
            cycle_id,
            entry,
            src,
            offset,
            timeout,
            bwlimit_kbps,
        )?,
        Err(err) => return Err(err),
    };
    // Catch torn streams: the hash covers what was read, not a consistent
    // version — a same-size mutation mid-stream would otherwise pass.
    ensure_source_stable(src, entry)?;
    let finish = TransferFinishFileRequest {
        root: destination_root.to_path_buf(),
        cycle_id,
        entry: entry.clone(),
        full_hash: Some(full_hash),
    };
    let ack: TransferAck =
        remote_post_json(destination, "/api/transfer/finish-file", &finish, timeout)?;
    if !ack.ok {
        bail!("peer rejected TCP file transfer");
    }
    Ok(())
}

/// Stream the file's remainder (from `offset`) as ONE sized request body,
/// hashing the WHOLE file on the way — bytes before the resume offset are
/// read for hashing only. Neither side ever buffers more than one read
/// chunk, unlike the legacy path's one 16 MiB request body per chunk on the
/// receiver. Returns the whole-file blake3 hex.
#[allow(clippy::too_many_arguments)]
fn send_file_body_streamed(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    offset: u64,
    timeout: Duration,
    bwlimit_kbps: u64,
) -> Result<String> {
    let total_size = entry.size.max(0) as u64;
    let mut file = File::open(src).with_context(|| format!("failed to read {}", src.display()))?;
    let mut hasher = blake3::Hasher::new();
    let api_path = format!(
        "/api/transfer/put-file-stream?root={}&rel_path={}&cycle_id={}&size={}&offset={}",
        encode_query_component(&destination_root.to_string_lossy()),
        encode_query_component(&entry.rel_path),
        cycle_id,
        entry.size,
        offset
    );
    let content_length = total_size - offset;
    let response = crate::core::machines::remote_post_octet_stream(
        destination,
        &api_path,
        content_length,
        &mut |out| {
            let mut pos = 0_u64;
            let mut buf = vec![0_u8; TRANSFER_CHUNK_SIZE];
            while pos < total_size {
                // Per-chunk poll so a multi-gigabyte file aborts mid-stream
                // instead of holding the cancel until the file completes.
                cancel::check()?;
                let remaining = (total_size - pos).min(TRANSFER_CHUNK_SIZE as u64) as usize;
                let n = file.read(&mut buf[..remaining])?;
                if n == 0 {
                    // The file shrank below its snapshot size mid-stream;
                    // canonical, parseable source-changing form.
                    bail!("source changed while copying {}", entry.rel_path);
                }
                hasher.update(&buf[..n]);
                let chunk_end = pos + n as u64;
                if chunk_end > offset {
                    let skip = offset.saturating_sub(pos) as usize;
                    out.write_all(&buf[skip..n])?;
                    let sent_now = n - skip;
                    progress::record_transfer(&entry.rel_path, sent_now as u64);
                    throttle_after_transfer(sent_now, bwlimit_kbps);
                }
                pos = chunk_end;
            }
            Ok(())
        },
        timeout,
    )?;
    let ack: TransferAck =
        serde_json::from_slice(&response).context("failed to parse peer response")?;
    if !ack.ok {
        bail!("peer rejected streamed file body");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Legacy big-file delivery: one 16 MiB request per chunk. Kept as the
/// fallback for peers without the put-file-stream endpoint.
#[allow(clippy::too_many_arguments)]
fn send_file_body_chunked(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    offset: u64,
    timeout: Duration,
    bwlimit_kbps: u64,
) -> Result<String> {
    let total_size = entry.size.max(0) as u64;
    let mut file = File::open(src).with_context(|| format!("failed to read {}", src.display()))?;
    // Read from the start so we can hash the whole file end-to-end, but only
    // send the bytes from `offset` onward (resume). The 16 MiB buffer bounds
    // memory regardless of file size.
    let mut hasher = blake3::Hasher::new();
    let mut pos = 0_u64;
    let mut buf = vec![0_u8; TRANSFER_CHUNK_SIZE];
    while pos < total_size {
        cancel::check()?;
        let remaining = (total_size - pos).min(TRANSFER_CHUNK_SIZE as u64) as usize;
        let n = file.read(&mut buf[..remaining])?;
        if n == 0 {
            bail!("source changed while copying {}", entry.rel_path);
        }
        hasher.update(&buf[..n]);
        let chunk_end = pos + n as u64;
        if chunk_end > offset {
            let skip = offset.saturating_sub(pos) as usize;
            let send_at = pos + skip as u64;
            let path = receive_file_chunk_api_path(destination_root, cycle_id, entry, send_at);
            let ack: TransferAck = remote_post_bytes(destination, &path, &buf[skip..n], timeout)?;
            if !ack.ok {
                bail!("peer rejected TCP file chunk");
            }
            let sent_now = n - skip;
            progress::record_transfer(&entry.rel_path, sent_now as u64);
            throttle_after_transfer(sent_now, bwlimit_kbps);
        }
        pos = chunk_end;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[allow(clippy::too_many_arguments)]
fn send_put_file_tcp(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    timeout: Duration,
    bwlimit_kbps: u64,
) -> Result<()> {
    let total_size = entry.size.max(0) as u64;
    let bytes = fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    if bytes.len() as u64 != total_size {
        // Canonical message: callers (possibly across the HTTP hop) parse it
        // via `source_changed_paths` to classify the failure as tolerable.
        warn!(
            rel_path = entry.rel_path,
            expected = total_size,
            read = bytes.len(),
            "source size changed while sending"
        );
        bail!("source changed while copying {}", entry.rel_path);
    }
    // Catch same-size torn reads too (mutation mid-read keeps the length).
    ensure_source_stable(src, entry)?;
    let _ = destination_id;
    let full_hash = blake3::hash(&bytes).to_hex().to_string();
    let path = put_file_api_path(destination_root, cycle_id, entry, &full_hash);
    let ack: TransferAck = remote_post_bytes(destination, &path, &bytes, timeout)?;
    if !ack.ok {
        bail!("peer rejected put-file transfer");
    }
    progress::record_transfer(&entry.rel_path, total_size);
    throttle_after_transfer(bytes.len(), bwlimit_kbps);
    Ok(())
}

/// Send a changed file as an rsync-style delta against the copy the destination
/// already holds. Falls back to a full transfer when the destination has no
/// usable basis or the delta would not be smaller.
#[allow(clippy::too_many_arguments)]
fn send_file_delta(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    timeout: Duration,
    bwlimit_kbps: u64,
) -> Result<()> {
    let sums_req = TransferBlockSumsRequest {
        root: destination_root.to_path_buf(),
        rel_path: entry.rel_path.clone(),
    };
    let sums: delta::BlockSums =
        remote_post_json(destination, "/api/transfer/block-sums", &sums_req, timeout)?;
    if sums.blocks.is_empty() {
        return send_file_tcp(
            destination,
            destination_root,
            destination_id,
            cycle_id,
            entry,
            src,
            timeout,
            bwlimit_kbps,
        );
    }

    // The sender holds the whole new file plus the encoded delta in memory;
    // bound the aggregate across parallel workers so a batch of large changed
    // files cannot balloon resident memory (see `acquire_transfer_memory`).
    let memory_permit = acquire_transfer_memory((entry.size.max(0) as u64).saturating_mul(2));
    let new_data = fs::read(src).with_context(|| format!("failed to read {}", src.display()))?;
    if new_data.len() as u64 != entry.size.max(0) as u64 {
        warn!(
            rel_path = entry.rel_path,
            expected = entry.size,
            read = new_data.len(),
            "source size changed while sending delta"
        );
        bail!("source changed while copying {}", entry.rel_path);
    }
    // Catch same-size torn reads too (mutation mid-read keeps the length).
    ensure_source_stable(src, entry)?;
    let delta_bytes = delta::build_delta(&new_data, &sums);
    // If the delta saves little, a plain chunked transfer avoids the basis read
    // on the destination; fall back unless we beat ~90% of the file size.
    if delta_bytes.len() as u64 >= new_data.len() as u64 / 10 * 9 {
        // Release the buffers (and their memory permit) before the chunked
        // send, which streams with a small buffer and can run for minutes.
        drop(delta_bytes);
        drop(new_data);
        drop(memory_permit);
        return send_file_tcp(
            destination,
            destination_root,
            destination_id,
            cycle_id,
            entry,
            src,
            timeout,
            bwlimit_kbps,
        );
    }
    let full_hash = blake3::hash(&new_data).to_hex().to_string();
    let path = apply_delta_api_path(destination_root, cycle_id, entry, &full_hash);
    let ack: TransferAck = remote_post_bytes(destination, &path, &delta_bytes, timeout)?;
    if !ack.ok {
        bail!("peer rejected delta transfer for {}", entry.rel_path);
    }
    progress::record_transfer(&entry.rel_path, entry.size.max(0) as u64);
    throttle_after_transfer(delta_bytes.len(), bwlimit_kbps);
    Ok(())
}

fn apply_delta_api_path(
    root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    full_hash: &str,
) -> String {
    format!(
        "/api/transfer/apply-delta?root={}&rel_path={}&cycle_id={}&size={}&mtime_ns={}&mode={}&full_hash={}",
        encode_query_component(&root.to_string_lossy()),
        encode_query_component(&entry.rel_path),
        cycle_id,
        entry.size,
        entry.mtime_ns,
        entry.mode,
        encode_query_component(full_hash)
    )
}

fn put_file_api_path(root: &Path, cycle_id: i64, entry: &SnapshotEntry, full_hash: &str) -> String {
    format!(
        "/api/transfer/put-file?root={}&rel_path={}&cycle_id={}&size={}&mtime_ns={}&mode={}&full_hash={}",
        encode_query_component(&root.to_string_lossy()),
        encode_query_component(&entry.rel_path),
        cycle_id,
        entry.size,
        entry.mtime_ns,
        entry.mode,
        encode_query_component(full_hash)
    )
}

fn send_symlink_tcp(
    destination: &crate::core::config::MachineConfig,
    destination_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    src: &Path,
    timeout: Duration,
) -> Result<()> {
    let raw_target =
        fs::read_link(src).with_context(|| format!("failed to read symlink {}", src.display()))?;
    // The link changed between the scan and this push: report the canonical
    // source-changing wording so the pass records a yellow retryable issue —
    // the receiver-side hash mismatch used to burn 3 retries and count as a
    // hard failure instead.
    if Some(hash_symlink_target(&raw_target)) != entry.hash {
        bail!("source changed while copying {}", entry.rel_path);
    }
    let req = TransferReceiveSymlinkRequest {
        root: destination_root.to_path_buf(),
        rel_path: entry.rel_path.clone(),
        cycle_id,
        mtime_ns: entry.mtime_ns,
        mode: entry.mode,
        hash: entry.hash.clone(),
        target: symlink_target_for_wire(&raw_target),
        // Unix-to-Unix keeps the exact bytes (a target is not required to be
        // UTF-8 there); the string field stays for Windows and old peers.
        #[cfg(unix)]
        target_bytes: Some(std::os::unix::ffi::OsStrExt::as_bytes(raw_target.as_os_str()).to_vec()),
        #[cfg(not(unix))]
        target_bytes: None,
        is_dir: symlink_points_to_dir(src),
    };
    let ack: TransferAck =
        remote_post_json(destination, "/api/transfer/receive-symlink", &req, timeout)?;
    if !ack.ok {
        bail!("peer rejected symlink transfer");
    }
    Ok(())
}

/// Wire form of a symlink target: Windows senders normalize `\` to `/` so a
/// RELATIVE target resolves on a Unix destination (and [`hash_symlink`]
/// normalizes the same way, keeping both sides' fingerprints equal). Unix
/// targets pass through untouched — `\` is a legal filename byte there.
fn symlink_target_for_wire(target: &Path) -> String {
    #[cfg(windows)]
    {
        target.to_string_lossy().replace('\\', "/")
    }
    #[cfg(not(windows))]
    {
        target.to_string_lossy().to_string()
    }
}

fn throttle_after_transfer(bytes: usize, bwlimit_kbps: u64) {
    if bwlimit_kbps == 0 || bytes == 0 {
        return;
    }
    let millis = ((bytes as u128) * 1000) / ((bwlimit_kbps as u128) * 1024);
    if millis > 0 {
        std::thread::sleep(Duration::from_millis(millis.min(u64::MAX as u128) as u64));
    }
}

fn receive_file_chunk_api_path(
    root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    offset: u64,
) -> String {
    format!(
        "/api/transfer/receive-file-chunk?root={}&rel_path={}&cycle_id={}&size={}&offset={}",
        encode_query_component(&root.to_string_lossy()),
        encode_query_component(&entry.rel_path),
        cycle_id,
        entry.size,
        offset
    )
}

fn safe_join_rel(root: &Path, rel_path: &str) -> Result<PathBuf> {
    let rel = normalize_rel_path(rel_path)?;
    Ok(root.join(rel))
}

fn normalize_rel_path(rel_path: &str) -> Result<PathBuf> {
    // All internal producers join components with '/'. Treat '\' as a
    // separator only where the OS does: on Linux it is a legal filename byte
    // (rewriting it silently redirected "a\b" to a nested a/b path — wrong
    // copies, phantom deletes under mirror).
    #[cfg(windows)]
    let normalized = rel_path.replace('\\', "/");
    #[cfg(not(windows))]
    let normalized = rel_path.to_string();
    let rel = Path::new(&normalized);
    if rel.is_absolute() {
        bail!("relative path is absolute: {rel_path}");
    }
    let mut out = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                // ':' is a drive-letter / alternate-data-stream hazard on Windows
                // but a perfectly valid filename byte on Linux/ZFS (e.g. Perl
                // man pages like "APR::Base64.3pm"), so only reject it there.
                #[cfg(windows)]
                if part.to_string_lossy().contains(':') {
                    bail!("unsafe relative path: {rel_path}");
                }
                out.push(part);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe relative path: {rel_path}");
            }
        }
    }
    if out.as_os_str().is_empty() {
        bail!("invalid empty relative path");
    }
    Ok(out)
}

/// True when this cycle's pass reads the whole source tree for at least one of
/// its targeted destinations (rather than replaying recorded event paths): a
/// full/event-loss rescan, or a destination whose first sync has no verified
/// baseline to compute an incremental against. Used only to label the task log.
fn cycle_runs_full_reconcile(state: &State, source: &SourceGroupConfig, cycle: &Cycle) -> bool {
    if cycle.needs_full_rescan {
        return true;
    }
    source
        .destinations
        .iter()
        .filter(|dst| dst.enabled)
        .any(|dst| {
            matches!(
                state.destination_offset(&source.id, &dst.id),
                Ok(offset)
                    if offset.target_cycle_id == Some(cycle.id)
                        && offset.last_verified_cycle_id.is_none()
            )
        })
}

pub fn sync_cycle_for_source(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<SyncCycleOutcome> {
    info!(
        source = source.id,
        cycle_id = cycle.id,
        needs_full_rescan = cycle.needs_full_rescan,
        "sync cycle started"
    );
    // Scope this cycle's work to the source's destinations: a stop request
    // targeted at any of them cancels the pass (prefetch walks and transfers
    // are shared across the cycle's destinations, so there is no smaller
    // stoppable unit), while other sources' passes stay untouched.
    let _cycle_scope = cancel::begin_targets(
        cancel::KIND_SYNC,
        source
            .destinations
            .iter()
            .filter(|dst| dst.enabled)
            .map(|dst| cancel::target_for(&source.id, &dst.id))
            .collect(),
    );
    // Task log: one row per cycle pass, opened while running (queryable
    // live), closed with the outcome; a pass that moved nothing is dropped
    // so the log holds real work, not scheduler heartbeats.
    let kind = current_sync_kind().unwrap_or_else(|| "incremental".to_string());
    // Task-log label only: an "incremental" pass that actually reads the whole
    // source tree (a destination's first sync has no baseline to replay events
    // against, and an event-loss / manual full rescan re-reads everything) is a
    // Full, not an event-replay Incremental. Labelling it "incremental" was
    // misleading in the Tasks list. The restart-notice check below still keys
    // off the untouched `kind` so only a manual Full clears it.
    let log_kind = if kind == "repair_scan" && cycle.manual_full_rescan {
        // A Compare-repair that escalated to a full reconcile because the stored
        // report was truncated (a difference kind exceeded the per-kind cap, so
        // the sample could not name every path). Record it as "repair_full" so
        // the Tasks list and the Info panel agree and the escalation is visible
        // — not a bare "Repair" (hiding that it went full) nor a bare "Full"
        // (hiding what triggered it).
        "repair_full"
    } else if kind == "incremental" && cycle_runs_full_reconcile(state, source, cycle) {
        "full"
    } else {
        kind.as_str()
    };
    let destination_ids = source
        .destinations
        .iter()
        .filter(|dst| dst.enabled)
        .map(|dst| dst.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let task_id = state
        .task_start(log_kind, &source.id, &destination_ids)
        .ok();
    // Phase indicator starts at "preparing"; the per-phase functions (zfs diff,
    // transferring, verifying) overwrite it as the pass advances.
    set_sync_phase("preparing");
    let action_started_at = chrono::Utc::now().to_rfc3339();
    let result = sync_cycle_for_source_inner(cfg, state, source, cycle);
    // A manual Full pass that began after a restart notice re-reads (or
    // zfs-diffs) the source tree, covering whatever the daemon's downtime
    // may have missed; a plain incremental replays only recorded events and
    // vouches for nothing.
    if result.is_ok() && kind == "full" {
        if let Err(err) = state.clear_restart_notice_if_covered(&source.id, &action_started_at) {
            warn!(source = source.id, error = %err, "failed to clear restart notice");
        }
    }
    if let Some(task_id) = task_id {
        let files = cancel::synced_files();
        // For a sync the task log's `differences` column carries the failed-file
        // count (a sync has no diff count of its own), so the Info summary can
        // report "N synced · M failed".
        let failed = cancel::failed_files();
        let record = match &result {
            Err(err) if cancel::error_is_cancelled(err) => state.task_finish(
                task_id,
                "cancelled",
                cancel::CANCELLED_MESSAGE,
                files,
                failed,
                0,
            ),
            Err(err) => state.task_finish(task_id, "failed", &format!("{err:#}"), files, failed, 0),
            // A pass that "failed" only because the live source changed under
            // the copy (destinations yellow `source_changing`, none red) copied
            // everything stable and deferred the changed paths to the next
            // cycle — log it as a warning, not a failure, and say why.
            Ok(_) if source_changing_only_pass(state, source, cycle.id).is_some() => {
                let n = source_changing_only_pass(state, source, cycle.id).unwrap_or(0);
                let reason = format!(
                    "{files} files synced; {n} destination(s) had files change during \
                     the copy — deferred to the next cycle"
                );
                state.task_finish(task_id, "warning", &reason, files, failed, 0)
            }
            // The inner pass returns Ok even when a destination went red (it
            // records the red status and marks the cycle failed, but does not
            // propagate an Err). Such a pass MUST be logged as failed — never
            // discarded as a no-op just because it transferred 0 files.
            Ok(_) if failed_pass_reason(state, source, cycle.id).is_some() => {
                let reason = failed_pass_reason(state, source, cycle.id).unwrap_or_default();
                state.task_finish(task_id, "failed", &reason, files, failed, 0)
            }
            Ok(outcome) if files == 0 && failed == 0 && !outcome.progressed => {
                state.task_discard(task_id)
            }
            Ok(_) => state.task_finish(task_id, "success", "", files, failed, 0),
        };
        if let Err(err) = record {
            warn!(source = source.id, error = %err, "failed to record task log entry");
        }
    }
    result
}

fn sync_cycle_for_source_inner(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<SyncCycleOutcome> {
    if cycle_has_remote_target(cfg, state, source, cycle)? {
        return sync_cycle_with_transfer(cfg, state, source, cycle);
    }

    let live_source_endpoint = match SourceEndpoint::resolve(source) {
        Ok(endpoint) => endpoint,
        Err(err) => {
            for dst in &source.destinations {
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    None,
                    "red",
                    "source_unavailable",
                )?;
            }
            state.mark_cycle_status(cycle.id, "failed")?;
            return Err(err)
                .with_context(|| format!("source path is unavailable: {}", source.src.display()));
        }
    };

    let mut all_verified = true;
    let mut targeted_count = 0_usize;
    let mut blocked_count = 0_usize;
    let mut had_unblocked_failure = false;
    let mut progressed = false;
    let mut ready_destinations = Vec::new();
    for (dst_index, dst) in source
        .destinations
        .iter()
        .enumerate()
        .filter(|(_, d)| d.enabled)
    {
        if state.destination_target_cycle(&source.id, &dst.id)? != Some(cycle.id) {
            continue;
        }
        targeted_count += 1;
        let last_verified = state.destination_last_verified(&source.id, &dst.id)?;
        if last_verified >= Some(cycle.id) {
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                Some(cycle.id),
                "green",
                "verified",
            )?;
            continue;
        }

        // Paused by the user: hold the pending target (resume picks it up
        // exactly where the pause left off) without driving any work.
        if dst.paused {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(&source.id, &dst.id, None, "yellow", "paused")?;
            continue;
        }

        // A compare for this destination is running: syncing changes under it
        // would skew its report, so hold the sync until it finishes (the
        // target stays; the next scheduler tick retries).
        if cancel::kind_target_active(
            cancel::KIND_COMPARE,
            &cancel::target_for(&source.id, &dst.id),
        ) {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "yellow",
                "waiting_for_compare",
            )?;
            continue;
        }

        // Cold-backup standby: if this destination lives on a parked pool
        // (outside its wake window), hold the target so the work backlogs and
        // the disk stays asleep. A source on a *different* standby pool is woken
        // on demand (not gated). Either way, refuse to run if a touched pool
        // root is not a real mount — a "sync" reading an empty tree would
        // mirror-delete the whole backup.
        if !cfg.standby_pools.is_empty() {
            match crate::core::standby::gate_for_sync(
                &cfg.standby_pools,
                source.src.as_path(),
                dst.path.as_path(),
                chrono::Local::now(),
            ) {
                Ok(Some(gate)) => {
                    if !standby_gate_blocks_sync(&gate, cycle.manual_full_rescan) {
                        info!(
                            source = source.id,
                            destination = dst.id,
                            gate = %gate.status_reason(),
                            "manual Full bypasses standby schedule gate"
                        );
                    } else {
                        all_verified = false;
                        let not_mounted =
                            matches!(gate, crate::core::standby::Gate::NotMounted { .. });
                        if not_mounted {
                            had_unblocked_failure = true;
                        } else {
                            blocked_count += 1;
                        }
                        state.upsert_destination_status(
                            &source.id,
                            &dst.id,
                            None,
                            if not_mounted { "red" } else { "yellow" },
                            &gate.status_reason(),
                        )?;
                        continue;
                    }
                }
                Ok(None) => {}
                // A malformed wake schedule must not wedge backups shut; log and
                // proceed rather than block on a config parse error.
                Err(err) => warn!(
                    source = source.id,
                    destination = dst.id,
                    error = %err,
                    "standby gate evaluation failed; proceeding"
                ),
            }
        }

        let dst_endpoint =
            match DestinationEndpoint::resolve(&live_source_endpoint, dst).and_then(|endpoint| {
                endpoint.check_online()?;
                Ok(endpoint)
            }) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    all_verified = false;
                    had_unblocked_failure = true;
                    warn!(
                        source = source.id,
                        destination = dst.id,
                        path = %dst.path.display(),
                        error = %err,
                        "destination offline"
                    );
                    state.upsert_destination_status(
                        &source.id,
                        &dst.id,
                        None,
                        "red",
                        &short_reason(&err),
                    )?;
                    continue;
                }
            };
        ready_destinations.push((dst_index, dst_endpoint));
    }

    if ready_destinations.is_empty() {
        if targeted_count == 0 || all_verified {
            state.mark_cycle_status(cycle.id, "verified")?;
        } else if blocked_count > 0 && !had_unblocked_failure {
            state.mark_cycle_status(cycle.id, "closed")?;
        } else {
            state.mark_cycle_status(cycle.id, "failed")?;
        }
        return Ok(SyncCycleOutcome {
            progressed: false,
            blocked: blocked_count > 0,
        });
    }

    let ready_indexes: Vec<usize> = ready_destinations
        .iter()
        .map(|(dst_index, _)| *dst_index)
        .collect();
    let source_checksum = any_ready_destination_needs_checksum(cfg, source, &ready_indexes);
    if let Some(plan) = event_incremental_plan(state, source, cycle, &ready_indexes)? {
        match plan {
            RealtimeIncrementalPlan::Unusable(reason) => {
                for dst_index in ready_indexes {
                    let dst = &source.destinations[dst_index];
                    state.clear_destination_issues(&source.id, &dst.id)?;
                    state.upsert_destination_status(&source.id, &dst.id, None, "yellow", reason)?;
                }
                state.mark_cycle_status(cycle.id, "failed")?;
                return Ok(SyncCycleOutcome {
                    progressed: false,
                    blocked: false,
                });
            }
            RealtimeIncrementalPlan::Apply(per_dst_paths) => {
                let paths_by_index: BTreeMap<usize, Vec<String>> =
                    per_dst_paths.into_iter().collect();
                state.mark_cycle_status(cycle.id, "syncing")?;
                for (dst_index, dst_endpoint) in ready_destinations {
                    let dst = &source.destinations[dst_index];
                    let sync = effective_sync_config(cfg, dst);
                    let empty = Vec::new();
                    let rel_paths = paths_by_index.get(&dst_index).unwrap_or(&empty);
                    // Watcher events carry no recursion info: treat every
                    // path as a full subtree (a created/renamed directory's
                    // children may not each have their own event).
                    match sync_endpoint_event_paths(
                        &live_source_endpoint,
                        &dst_endpoint,
                        &dst.id,
                        cycle.id,
                        &diff_paths_all_recursive(rel_paths),
                        &[],
                        &source.excludes,
                        &sync,
                    ) {
                        Ok(()) => {
                            progressed |= !rel_paths.is_empty();
                            state.clear_destination_issues(&source.id, &dst.id)?;
                            state.upsert_destination_status(
                                &source.id,
                                &dst.id,
                                Some(cycle.id),
                                "green",
                                "verified",
                            )?;
                        }
                        Err(err) => {
                            all_verified = false;
                            had_unblocked_failure = true;
                            record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
                        }
                    }
                }
                if targeted_count == 0 || all_verified {
                    state.mark_cycle_status(cycle.id, "verified")?;
                } else if blocked_count > 0 && !had_unblocked_failure {
                    state.mark_cycle_status(cycle.id, "closed")?;
                } else {
                    state.mark_cycle_status(cycle.id, "failed")?;
                }
                return Ok(SyncCycleOutcome {
                    progressed,
                    blocked: blocked_count > 0,
                });
            }
        }
    }

    // A reconcile reached here because of a possible-event-loss signal (overflow,
    // USN/journal gap, startup gap) rather than a user action. Mark the affected
    // destinations red and identify the reason while the reconcile runs; each one
    // returns to green only after its full re-scan verifies.
    let is_event_loss_reconcile = cycle.needs_full_rescan && !cycle.manual_full_rescan;
    if is_event_loss_reconcile {
        warn!(
            source = source.id,
            cycle_id = cycle.id,
            "possible event loss detected; running reconcile to repair destinations"
        );
        for &dst_index in &ready_indexes {
            let dst = &source.destinations[dst_index];
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "red",
                "event_loss_reconcile",
            )?;
        }
    }

    let source_view = SourceReadView::prepare(
        source,
        &live_source_endpoint,
        cycle.id,
        Some(cycle.starts_at.timestamp()),
    )?;
    let source_endpoint = source_view.endpoint.clone();

    info!(
        source = source.id,
        cycle_id = cycle.id,
        "reconcile: source view ready, marking cycle syncing"
    );
    state.mark_cycle_status(cycle.id, "planning")?;
    state.mark_cycle_status(cycle.id, "syncing")?;
    info!(
        source = source.id,
        cycle_id = cycle.id,
        ready = ready_destinations.len(),
        "reconcile: entering destination loop"
    );
    let mut shared_source_snapshot: Option<Vec<SnapshotEntry>> = None;
    for (dst_index, dst_endpoint) in ready_destinations {
        let dst = &source.destinations[dst_index];
        let sync = effective_sync_config(cfg, dst);
        info!(
            source = source.id,
            destination = dst.id,
            cycle_id = cycle.id,
            "reconcile: processing destination"
        );
        if let (
            SourceEndpoint::Dir { root: src_root, .. },
            DestinationEndpoint::Dir { root: dst_root },
        ) = (&source_endpoint, &dst_endpoint)
        {
            {
                // ZFS diff incremental: when this is a ZFS source and the
                // destination still has its retained base snapshot, sync only
                // the paths `zfs diff` reports instead of re-scanning the tree.
                // Skipped for event-loss reconciles, which must re-verify the
                // whole destination. Falls back to a full reconcile on any
                // failure.
                if sync.zfs_diff && !is_event_loss_reconcile && !cycle.manual_full_rescan {
                    if let Some(zfs) = source_view.zfs_snapshot.as_ref() {
                        if let Some(base) =
                            state.destination_verified_snapshot(&source.id, &dst.id)?
                        {
                            if let Some(rel_paths) =
                                zfs_diff_changed_paths(&base, &zfs.full_name, &zfs.source_live_root)
                            {
                                info!(
                                    source = source.id,
                                    destination = dst.id,
                                    cycle_id = cycle.id,
                                    base = base,
                                    changed = rel_paths.len(),
                                    "zfs diff incremental sync"
                                );
                                match sync_endpoint_event_paths(
                                    &source_endpoint,
                                    &dst_endpoint,
                                    &dst.id,
                                    cycle.id,
                                    &rel_paths,
                                    &[],
                                    &source.excludes,
                                    &sync,
                                ) {
                                    Ok(()) => {
                                        progressed |= !rel_paths.is_empty();
                                        state.clear_destination_issues(&source.id, &dst.id)?;
                                        state.upsert_destination_status(
                                            &source.id,
                                            &dst.id,
                                            Some(cycle.id),
                                            "green",
                                            "verified",
                                        )?;
                                        // Source-side pass only: the dst
                                        // baseline must NOT be refreshed here
                                        // (it would hide dst drift from
                                        // future zfs-diff Compares/Fulls).
                                        record_destination_verified_baselines(
                                            state,
                                            source,
                                            &dst.id,
                                            Some(&zfs.full_name),
                                            DstBaselineAction::Retain,
                                            cycle.id,
                                        )?;
                                        continue;
                                    }
                                    Err(err) => {
                                        warn!(
                                            source = source.id,
                                            destination = dst.id,
                                            error = %err,
                                            "zfs diff incremental failed; falling back to full reconcile"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                // Manual Full via dual-side zfs diff: with BOTH verified
                // baselines, the union of each side's changes since its base
                // is the complete set of paths that can differ now (dst-side
                // drift included) — everything else was verified identical at
                // the baselines and untouched since. Reconciling just the
                // union equals a full pass at a fraction of the IO.
                if sync.zfs_diff && cycle.manual_full_rescan {
                    if let Some(zfs) = source_view.zfs_snapshot.as_ref() {
                        if let Some((rel_paths, dst_only)) =
                            full_zfs_diff_paths(state, source, &dst.id, zfs, dst_root)?
                        {
                            info!(
                                source = source.id,
                                destination = dst.id,
                                cycle_id = cycle.id,
                                changed = rel_paths.len(),
                                "manual Full served by dual-side zfs diff"
                            );
                            match sync_endpoint_event_paths(
                                &source_endpoint,
                                &dst_endpoint,
                                &dst.id,
                                cycle.id,
                                &rel_paths,
                                &dst_only,
                                &source.excludes,
                                &sync,
                            ) {
                                Ok(()) => {
                                    progressed |= !rel_paths.is_empty();
                                    state.clear_destination_issues(&source.id, &dst.id)?;
                                    state.upsert_destination_status(
                                        &source.id,
                                        &dst.id,
                                        Some(cycle.id),
                                        "green",
                                        "verified",
                                    )?;
                                    let touched: BTreeSet<String> =
                                        rel_paths.iter().map(|path| path.rel.clone()).collect();
                                    record_destination_verified_baselines(
                                        state,
                                        source,
                                        &dst.id,
                                        Some(&zfs.full_name),
                                        DstBaselineAction::Refresh {
                                            dst_root,
                                            touched: Some(&touched),
                                        },
                                        cycle.id,
                                    )?;
                                    continue;
                                }
                                Err(err) => {
                                    warn!(
                                        source = source.id,
                                        destination = dst.id,
                                        error = %err,
                                        "zfs diff Full failed; falling back to walk-based full reconcile"
                                    );
                                }
                            }
                        }
                    }
                }
                // Reaching here means no zfs diff base was usable, so this is a
                // full source+dst reconcile; reflect that in the status type.
                let _kind = set_sync_kind("full");
                info!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    "reconcile: starting full reconcile (fast_missing_dirs)"
                );
                let sync_result = sync_destination_fast_missing_dirs(
                    src_root,
                    dst_root,
                    &dst.id,
                    cycle.id,
                    &source.excludes,
                    &sync,
                );
                match sync_result {
                    Ok((_source_snapshot, touched)) => {
                        progressed = true;
                        state.clear_destination_issues(&source.id, &dst.id)?;
                        state.upsert_destination_status(
                            &source.id,
                            &dst.id,
                            Some(cycle.id),
                            "green",
                            "verified",
                        )?;
                        // Record the base snapshots for the next zfs diff (or
                        // clear them for non-ZFS sources so no stale base lingers).
                        record_destination_verified_baselines(
                            state,
                            source,
                            &dst.id,
                            source_view
                                .zfs_snapshot
                                .as_ref()
                                .map(|z| z.full_name.as_str()),
                            if sync.zfs_diff {
                                DstBaselineAction::Refresh {
                                    dst_root,
                                    touched: Some(&touched),
                                }
                            } else {
                                DstBaselineAction::Clear
                            },
                            cycle.id,
                        )?;
                        info!(
                            source = source.id,
                            destination = dst.id,
                            cycle_id = cycle.id,
                            "destination verified"
                        );
                    }
                    Err(err) => {
                        all_verified = false;
                        had_unblocked_failure = true;
                        error!(
                            source = source.id,
                            destination = dst.id,
                            cycle_id = cycle.id,
                            error = %err,
                            "destination sync failed"
                        );
                        record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
                    }
                }
                continue;
            }
        }
        let source_snapshot = if let Some(snapshot) = shared_source_snapshot.as_ref() {
            snapshot
        } else {
            let snapshot = source_endpoint
                .snapshot(&source.excludes, source_checksum)
                .with_context(|| format!("failed to snapshot source {}", source.src.display()))?;
            shared_source_snapshot = Some(snapshot);
            shared_source_snapshot.as_ref().unwrap()
        };
        let sync_result = sync_endpoint(
            &source_endpoint,
            &dst_endpoint,
            &dst.id,
            cycle.id,
            source_snapshot,
            &source.excludes,
            &sync,
        );
        match sync_result {
            Ok(()) => {
                progressed = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    Some(cycle.id),
                    "green",
                    "verified",
                )?;
                // Advance (or clear) the zfs diff bases like the other success
                // paths do; leaving a stale base forces redundant re-syncs.
                record_destination_verified_baselines(
                    state,
                    source,
                    &dst.id,
                    source_view
                        .zfs_snapshot
                        .as_ref()
                        .map(|z| z.full_name.as_str()),
                    // File sources have no zfs-diff consumer (the Compare and
                    // Full fast paths require a directory source): a dst
                    // baseline here would pin the destination dataset's whole
                    // churn for nothing.
                    DstBaselineAction::Clear,
                    cycle.id,
                )?;
                info!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    "destination verified"
                );
            }
            Err(err) => {
                all_verified = false;
                had_unblocked_failure = true;
                error!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    error = %err,
                    "destination sync failed"
                );
                record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
            }
        }
    }

    if targeted_count == 0 || all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
        let referenced = state.source_referenced_snapshots(&source.id)?;
        source_view.cleanup(source, &referenced);
    } else if blocked_count > 0 && !had_unblocked_failure {
        state.mark_cycle_status(cycle.id, "closed")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(SyncCycleOutcome {
        progressed,
        blocked: blocked_count > 0,
    })
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncCycleOutcome {
    pub progressed: bool,
    pub blocked: bool,
}

enum RealtimeIncrementalPlan {
    /// Per ready-destination accumulated event paths: `(dst_index, rel_paths)`.
    /// Realtime destinations track every cycle so their backlog is just the
    /// target cycle's events; scheduled destinations accumulate events across
    /// every cycle since their last verified one and apply them all at their
    /// schedule time.
    Apply(Vec<(usize, Vec<String>)>),
    Unusable(&'static str),
}

fn event_incremental_plan(
    state: &State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
    ready_destinations: &[usize],
) -> Result<Option<RealtimeIncrementalPlan>> {
    if ready_destinations.is_empty() {
        return Ok(None);
    }
    if cycle.manual_full_rescan {
        return Ok(None);
    }
    if cycle.needs_full_rescan {
        // A possible-event-loss signal (queue overflow, USN gap). Fall through
        // to a full reconcile that re-scans source+dst and repairs every
        // difference (incl. deletes the event stream may have missed) instead
        // of rubber-stamping green from an incomplete event backlog.
        return Ok(None);
    }

    let mut plans = Vec::with_capacity(ready_destinations.len());
    for &dst_index in ready_destinations {
        let dst = &source.destinations[dst_index];
        let Some(last_verified) = state.destination_last_verified(&source.id, &dst.id)? else {
            // First sync must be a full pass.
            return Ok(None);
        };
        let events = state.events_between_cycles(&source.id, last_verified, cycle.id)?;
        let actionable: Vec<&CycleEvent> = events
            .iter()
            .filter(|event| event.rel_path.is_some() || event.rescan_required)
            .collect();
        if actionable.iter().any(|event| event.rescan_required) {
            return Ok(None);
        }
        let mut paths = BTreeSet::new();
        for event in actionable {
            let Some(rel_path) = event.rel_path.as_deref() else {
                return Ok(Some(RealtimeIncrementalPlan::Unusable(
                    "realtime_event_path_unavailable",
                )));
            };
            let rel = normalize_rel_path(rel_path)
                .with_context(|| format!("invalid event path in cycle {}: {rel_path}", cycle.id))?;
            paths.insert(rel_to_string(&rel)?);
        }
        plans.push((dst_index, paths.into_iter().collect()));
    }
    Ok(Some(RealtimeIncrementalPlan::Apply(plans)))
}

/// The union of paths either side changed since its verified baseline
/// snapshot — the complete set of paths a manual Full needs to reconcile
/// (source side diffed snapshot-to-snapshot against the cycle's stable read
/// view, destination side diffed base-to-live). `Ok(None)` when a
/// precondition is missing (no baselines, dataset mismatch, dst not local
/// ZFS, zfs failure): the caller runs the walk-based full reconcile.
fn full_zfs_diff_paths(
    state: &State,
    source: &SourceGroupConfig,
    destination_id: &str,
    zfs: &ZfsSnapshot,
    dst_root: &Path,
) -> Result<Option<(Vec<DiffPath>, Vec<DiffPath>)>> {
    let Some(src_base) = state.destination_verified_snapshot(&source.id, destination_id)? else {
        return Ok(None);
    };
    let Some(dst_base) = state.destination_verified_dst_snapshot(&source.id, destination_id)?
    else {
        return Ok(None);
    };
    if src_base.split('@').next() != Some(zfs.dataset.as_str()) {
        return Ok(None);
    }
    let Ok(dst_dataset) = resolve_dataset_for_path(dst_root) else {
        return Ok(None);
    };
    if dst_base.split('@').next() != Some(dst_dataset.name.as_str()) {
        return Ok(None);
    }
    let Ok(dst_live_root) = dst_root.canonicalize() else {
        return Ok(None);
    };
    let Some(src_changed) =
        zfs_diff_changed_paths(&src_base, &zfs.full_name, &zfs.source_live_root)
    else {
        return Ok(None);
    };
    let Some(dst_changed) = zfs_diff_changed_paths_live(&dst_base, &dst_live_root) else {
        return Ok(None);
    };
    Ok(Some(diff_union_and_dst_only(src_changed, dst_changed)))
}

fn cycle_has_remote_target(
    cfg: &AppConfig,
    state: &State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<bool> {
    // A destination on THIS machine (even if it's labelled with the host's name
    // rather than "local") must use the local ZFS-snapshot path, not the
    // cross-machine transfer path against ourselves.
    if !machine_is_local(cfg, &source.machine_id) {
        return Ok(true);
    }
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if state.destination_target_cycle(&source.id, &dst.id)? == Some(cycle.id)
            && !machine_is_local(cfg, &dst.machine_id)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn sync_cycle_with_transfer(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
) -> Result<SyncCycleOutcome> {
    info!(
        source = source.id,
        cycle_id = cycle.id,
        "incremental transfer cycle started"
    );

    let source_machine_id = machine_id_or_local(&source.machine_id);
    let source_machine = find_machine(cfg, source_machine_id)
        .ok_or_else(|| anyhow!("unknown source machine: {source_machine_id}"))?;
    let source_info = match path_info_on_machine(source_machine_id, &source_machine, &source.src) {
        Ok(info) => info,
        Err(err) => {
            for dst in &source.destinations {
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    None,
                    "red",
                    "source_unavailable",
                )?;
            }
            state.mark_cycle_status(cycle.id, "failed")?;
            return Err(err)
                .with_context(|| format!("source path is unavailable: {}", source.src.display()));
        }
    };

    if source_info.kind != "dir" {
        return sync_cycle_file_with_transfer(cfg, state, source, cycle, &source_info);
    }

    let mut all_verified = true;
    let mut targeted_count = 0_usize;
    let mut blocked_count = 0_usize;
    let mut had_unblocked_failure = false;
    let mut progressed = false;
    let mut ready_destinations = Vec::new();

    for (dst_index, dst) in source
        .destinations
        .iter()
        .enumerate()
        .filter(|(_, d)| d.enabled)
    {
        if state.destination_target_cycle(&source.id, &dst.id)? != Some(cycle.id) {
            continue;
        }
        targeted_count += 1;
        let last_verified = state.destination_last_verified(&source.id, &dst.id)?;
        if last_verified >= Some(cycle.id) {
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                Some(cycle.id),
                "green",
                "verified",
            )?;
            continue;
        }
        // Paused by the user: hold the pending target (resume picks it up
        // exactly where the pause left off) without driving any work.
        if dst.paused {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(&source.id, &dst.id, None, "yellow", "paused")?;
            continue;
        }
        // A compare for this destination is running: syncing changes under it
        // would skew its report, so hold the sync until it finishes (the
        // target stays; the next scheduler tick retries).
        if cancel::kind_target_active(
            cancel::KIND_COMPARE,
            &cancel::target_for(&source.id, &dst.id),
        ) {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "yellow",
                "waiting_for_compare",
            )?;
            continue;
        }
        ready_destinations.push(dst_index);
    }

    if ready_destinations.is_empty() {
        if targeted_count == 0 || all_verified {
            state.mark_cycle_status(cycle.id, "verified")?;
        } else if blocked_count > 0 && !had_unblocked_failure {
            state.mark_cycle_status(cycle.id, "closed")?;
        } else {
            state.mark_cycle_status(cycle.id, "failed")?;
        }
        return Ok(SyncCycleOutcome {
            progressed: false,
            blocked: blocked_count > 0,
        });
    }

    if let Some(plan) = event_incremental_plan(state, source, cycle, &ready_destinations)? {
        match plan {
            RealtimeIncrementalPlan::Unusable(reason) => {
                for dst_index in ready_destinations {
                    let dst = &source.destinations[dst_index];
                    state.clear_destination_issues(&source.id, &dst.id)?;
                    state.upsert_destination_status(&source.id, &dst.id, None, "yellow", reason)?;
                }
                state.mark_cycle_status(cycle.id, "failed")?;
                return Ok(SyncCycleOutcome {
                    progressed: false,
                    blocked: false,
                });
            }
            RealtimeIncrementalPlan::Apply(per_dst_paths) if source_info.kind == "dir" => {
                let paths_by_index: BTreeMap<usize, Vec<String>> =
                    per_dst_paths.into_iter().collect();
                state.mark_cycle_status(cycle.id, "syncing")?;
                for dst_index in ready_destinations {
                    let dst = &source.destinations[dst_index];
                    let sync = effective_sync_config(cfg, dst);
                    let empty = Vec::new();
                    let rel_paths = paths_by_index.get(&dst_index).unwrap_or(&empty);
                    let dst_machine_id = machine_id_or_local(&dst.machine_id);
                    let dst_machine = match find_machine(cfg, dst_machine_id) {
                        Some(machine) => machine,
                        None => {
                            all_verified = false;
                            had_unblocked_failure = true;
                            state.upsert_destination_status(
                                &source.id,
                                &dst.id,
                                None,
                                "red",
                                "unknown_destination_machine",
                            )?;
                            continue;
                        }
                    };
                    let dst_root =
                        destination_root_for_source(source, &source_info, &dst.path, &dst_machine);
                    // Watcher events carry no recursion info: treat every
                    // path as a full subtree.
                    match sync_directory_event_paths_with_transfer(
                        source_machine_id,
                        &source_machine,
                        &source_info.base,
                        dst_machine_id,
                        &dst_machine,
                        &dst_root,
                        &dst.id,
                        cycle.id,
                        &diff_paths_all_recursive(rel_paths),
                        &source.excludes,
                        &sync,
                    ) {
                        Ok(()) => {
                            progressed |= !rel_paths.is_empty();
                            state.clear_destination_issues(&source.id, &dst.id)?;
                            state.upsert_destination_status(
                                &source.id,
                                &dst.id,
                                Some(cycle.id),
                                "green",
                                "verified",
                            )?;
                        }
                        Err(err) => {
                            all_verified = false;
                            had_unblocked_failure = true;
                            record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
                        }
                    }
                }
                if targeted_count == 0 || all_verified {
                    state.mark_cycle_status(cycle.id, "verified")?;
                } else if blocked_count > 0 && !had_unblocked_failure {
                    state.mark_cycle_status(cycle.id, "closed")?;
                } else {
                    state.mark_cycle_status(cycle.id, "failed")?;
                }
                return Ok(SyncCycleOutcome {
                    progressed,
                    blocked: blocked_count > 0,
                });
            }
            RealtimeIncrementalPlan::Apply(_) => {
                for dst_index in ready_destinations {
                    let dst = &source.destinations[dst_index];
                    state.clear_destination_issues(&source.id, &dst.id)?;
                    state.upsert_destination_status(
                        &source.id,
                        &dst.id,
                        None,
                        "yellow",
                        "realtime_file_source_needs_event_sync",
                    )?;
                }
                state.mark_cycle_status(cycle.id, "failed")?;
                return Ok(SyncCycleOutcome {
                    progressed: false,
                    blocked: false,
                });
            }
        }
    }

    // Possible-event-loss reconcile (overflow / journal gap / startup gap):
    // mark destinations red and identify the reason while the cross-machine
    // re-scan runs; each returns to green only after it verifies.
    let is_event_loss_reconcile = cycle.needs_full_rescan && !cycle.manual_full_rescan;
    if is_event_loss_reconcile {
        warn!(
            source = source.id,
            cycle_id = cycle.id,
            "possible event loss detected; running reconcile to repair destinations"
        );
        for &dst_index in &ready_destinations {
            let dst = &source.destinations[dst_index];
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "red",
                "event_loss_reconcile",
            )?;
        }
    }

    let source_checksum = any_ready_destination_needs_checksum(cfg, source, &ready_destinations);
    let source_timeout = ready_destination_timeout(cfg, source, &ready_destinations)
        .max(snapshot_timeout_floor(source_checksum));
    state.mark_cycle_status(cycle.id, "planning")?;

    // Stable read view when the source lives on this machine: reads come from
    // a ZFS snapshot (immutable), which both eliminates mid-copy source-change
    // races at the root and enables `zfs diff` incremental planning against the
    // base snapshot each destination last verified. Remote sources are read
    // live (their snapshotting would have to run on the remote machine).
    let source_view = if source_machine_id == "local" {
        let live_endpoint = SourceEndpoint::Dir {
            root: source_info.base.clone(),
            add_directory: source.add_directory,
        };
        Some(SourceReadView::prepare(
            source,
            &live_endpoint,
            cycle.id,
            Some(cycle.starts_at.timestamp()),
        )?)
    } else {
        None
    };
    let read_root: PathBuf = match source_view.as_ref().map(|view| &view.endpoint) {
        Some(SourceEndpoint::Dir { root, .. }) => root.clone(),
        Some(SourceEndpoint::File { path }) => path.clone(),
        None => source_info.base.clone(),
    };
    let zfs_snapshot = source_view
        .as_ref()
        .and_then(|view| view.zfs_snapshot.as_ref());

    state.mark_cycle_status(cycle.id, "syncing")?;
    // The full source snapshot is taken lazily: when every destination syncs
    // via `zfs diff` the whole-tree source scan is skipped entirely.
    let mut full_source_snapshot: Option<Vec<SnapshotEntry>> = None;
    for dst_index in ready_destinations {
        let dst = &source.destinations[dst_index];
        let sync = effective_sync_config(cfg, dst);
        let dst_machine_id = machine_id_or_local(&dst.machine_id);
        let dst_machine = match find_machine(cfg, dst_machine_id) {
            Some(machine) => machine,
            None => {
                all_verified = false;
                had_unblocked_failure = true;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    None,
                    "red",
                    "unknown_destination_machine",
                )?;
                continue;
            }
        };
        let dst_root = destination_root_for_source(source, &source_info, &dst.path, &dst_machine);

        // ZFS diff incremental (mirrors the local path): sync only the paths
        // `zfs diff` reports against the destination's verified base snapshot.
        // Skipped for event-loss and manual Full reconciles, which must
        // re-verify the whole destination; falls back to a full transfer on
        // any failure.
        if sync.zfs_diff && !is_event_loss_reconcile && !cycle.manual_full_rescan {
            if let Some(zfs) = zfs_snapshot {
                if let Some(base) = state.destination_verified_snapshot(&source.id, &dst.id)? {
                    if let Some(rel_paths) =
                        zfs_diff_changed_paths(&base, &zfs.full_name, &zfs.source_live_root)
                    {
                        info!(
                            source = source.id,
                            destination = dst.id,
                            cycle_id = cycle.id,
                            base = base,
                            changed = rel_paths.len(),
                            "zfs diff incremental transfer sync"
                        );
                        match sync_directory_event_paths_with_transfer(
                            source_machine_id,
                            &source_machine,
                            &read_root,
                            dst_machine_id,
                            &dst_machine,
                            &dst_root,
                            &dst.id,
                            cycle.id,
                            &rel_paths,
                            &source.excludes,
                            &sync,
                        ) {
                            Ok(()) => {
                                progressed |= !rel_paths.is_empty();
                                state.clear_destination_issues(&source.id, &dst.id)?;
                                state.upsert_destination_status(
                                    &source.id,
                                    &dst.id,
                                    Some(cycle.id),
                                    "green",
                                    "verified",
                                )?;
                                // A mixed cycle (some remote target) routes
                                // ALL destinations through this path: a LOCAL
                                // ZFS dst keeps its baseline exactly like the
                                // local incremental (source-side-only pass →
                                // Retain); clearing it here killed the
                                // dual-side fast paths on every mixed cycle.
                                record_destination_verified_baselines(
                                    state,
                                    source,
                                    &dst.id,
                                    Some(&zfs.full_name),
                                    if dst_machine_id == "local" {
                                        DstBaselineAction::Retain
                                    } else {
                                        DstBaselineAction::Clear
                                    },
                                    cycle.id,
                                )?;
                                continue;
                            }
                            Err(err) => {
                                warn!(
                                    source = source.id,
                                    destination = dst.id,
                                    error = %err,
                                    "zfs diff incremental transfer failed; falling back to full transfer"
                                );
                            }
                        }
                    }
                }
            }
        }

        // Full source+dst transfer pass; reflect that in the status type.
        let _kind = set_sync_kind("full");
        // The source and destination trees are independent (usually on
        // different machines): scan them CONCURRENTLY instead of serially,
        // roughly halving the reconcile's compare phase. The destination
        // prescan is skipped when the source snapshot is already cached
        // (nothing left to overlap with).
        let mut prefetched_dst: Option<Vec<SnapshotEntry>> = None;
        let source_snapshot = if let Some(snapshot) = full_source_snapshot.as_ref() {
            snapshot
        } else {
            let cancel_token = cancel::current_token();
            let (source_result, dst_result) = thread::scope(|scope| {
                let dst_handle = Some({
                    scope.spawn(|| {
                        let _cancel = cancel::enter(cancel_token.clone());
                        snapshot_on_machine(
                            dst_machine_id,
                            &dst_machine,
                            &dst_root,
                            TransferSnapshotMode::Destination,
                            &[],
                            sync.checksum,
                            snapshot_timeout(&sync),
                        )
                    })
                });
                let source_result = snapshot_on_machine(
                    source_machine_id,
                    &source_machine,
                    &read_root,
                    TransferSnapshotMode::Source,
                    &source.excludes,
                    source_checksum,
                    source_timeout,
                );
                let dst_result = dst_handle
                    .map(|handle| handle.join().expect("destination scan thread panicked"));
                (source_result, dst_result)
            });
            let snapshot = source_result
                .with_context(|| format!("failed to snapshot source {}", source.src.display()))?;
            if let Some(dst_result) = dst_result {
                prefetched_dst = Some(dst_result.with_context(|| {
                    format!("failed to snapshot destination {}", dst_root.display())
                })?);
            }
            full_source_snapshot = Some(snapshot);
            full_source_snapshot
                .as_ref()
                .expect("full source snapshot was just stored")
        };
        info!(
            source = source.id,
            destination = dst.id,
            cycle_id = cycle.id,
            "syncing destination with TCP incremental transfer"
        );
        let sync_result = sync_directory_with_transfer(
            source_machine_id,
            &source_machine,
            &read_root,
            dst_machine_id,
            &dst_machine,
            &dst_root,
            &dst.id,
            cycle.id,
            source_snapshot,
            prefetched_dst.take(),
            &source.excludes,
            &sync,
        );
        match sync_result {
            Ok(()) => {
                progressed = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    Some(cycle.id),
                    "green",
                    "verified",
                )?;
                // Record the base snapshot for the next zfs diff (or clear it
                // for non-ZFS sources so no stale base lingers). A LOCAL dst
                // in this mixed cycle just had whole-tree equality established
                // by the full manifest pass, which is exactly what a baseline
                // Refresh requires; only truly remote destinations have no
                // local dataset to snapshot.
                record_destination_verified_baselines(
                    state,
                    source,
                    &dst.id,
                    zfs_snapshot.map(|zfs| zfs.full_name.as_str()),
                    if dst_machine_id == "local" && sync.zfs_diff {
                        // No touched set threaded out of the transfer pass
                        // yet: the refresh proceeds unchecked here.
                        DstBaselineAction::Refresh {
                            dst_root: &dst_root,
                            touched: None,
                        }
                    } else {
                        DstBaselineAction::Clear
                    },
                    cycle.id,
                )?;
            }
            Err(err) => {
                all_verified = false;
                had_unblocked_failure = true;
                error!(
                    source = source.id,
                    destination = dst.id,
                    cycle_id = cycle.id,
                    error = %err,
                    "destination sync failed"
                );
                record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
            }
        }
    }

    if targeted_count == 0 || all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
        if let Some(view) = &source_view {
            let referenced = state.source_referenced_snapshots(&source.id)?;
            view.cleanup(source, &referenced);
        }
    } else if blocked_count > 0 && !had_unblocked_failure {
        state.mark_cycle_status(cycle.id, "closed")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(SyncCycleOutcome {
        progressed,
        blocked: blocked_count > 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn sync_directory_with_transfer(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine_id: &str,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    prefetched_dst_snapshot: Option<Vec<SnapshotEntry>>,
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    let timeout = transfer_timeout(sync);
    prepare_dir_on_machine(dst_machine_id, dst_machine, dst_root, None, None, timeout)?;
    // The caller usually prefetched the destination scan concurrently with the
    // source scan (a missing destination root scans as empty, so prefetching
    // before prepare-dir is safe); scan here only when it did not.
    let dst_snapshot = match prefetched_dst_snapshot {
        Some(snapshot) => snapshot,
        None => snapshot_on_machine(
            dst_machine_id,
            dst_machine,
            dst_root,
            TransferSnapshotMode::Destination,
            &[],
            sync.checksum,
            // Whole-tree walk on the peer: the per-file timeout is far too small.
            snapshot_timeout(sync),
        )?,
    };
    // Same compare implementation the Scan report uses.
    let diff = diff_manifests(source_snapshot, &dst_snapshot, excludes, sync, false);

    // 1. Remove destination entries whose type no longer matches the source
    //    (e.g. a file that is now a directory). Deepest paths first.
    let mut type_mismatch: Vec<String> = diff
        .type_mismatch
        .iter()
        .map(|entry| entry.rel_path.clone())
        .collect();
    type_mismatch.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
    remove_paths_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        &type_mismatch,
        cycle_id,
        timeout,
    )
    .context("failed to replace destination paths whose type changed")?;

    // 2. Create every needed directory in one bulk request (parents first),
    //    replacing one HTTP round-trip per directory.
    let mut dirs: Vec<TransferDirSpec> = source_snapshot
        .iter()
        .filter(|entry| entry.file_type == "dir")
        .map(|entry| TransferDirSpec {
            rel_path: entry.rel_path.clone(),
            mode: entry.mode,
            mtime_ns: entry.mtime_ns,
        })
        .collect();
    dirs.sort_by(|a, b| {
        path_depth(&a.rel_path)
            .cmp(&path_depth(&b.rel_path))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    prepare_dirs_on_machine(dst_machine_id, dst_machine, dst_root, &dirs, timeout)
        .context("failed to create destination directories")?;

    // 3. Transfer changed/missing files and symlinks concurrently. A file that
    //    already exists on the destination (same type) is eligible for an
    //    rsync-style delta against the copy that is there.
    let pending: Vec<(&SnapshotEntry, bool)> = diff
        .entries_to_copy()
        .into_iter()
        .map(|(entry, existing)| {
            (
                entry,
                existing.is_some_and(|existing| should_attempt_delta(entry, existing)),
            )
        })
        .collect();
    // Publish the plan so the UI can show live "synced X / Y to copy · Z
    // unchanged (N total)" during the transfer (a big Full is otherwise a long
    // silent "syncing…").
    set_sync_plan(
        source_snapshot.len() as u64,
        pending.len() as u64,
        diff.in_sync,
    );
    let transfer_started = Instant::now();
    let outcome = push_entries_parallel(
        source_machine_id,
        source_machine,
        source_root,
        dst_machine,
        dst_root,
        destination_id,
        cycle_id,
        &pending,
        sync,
    )?;
    info!(
        destination = destination_id,
        cycle_id,
        dirs = dirs.len(),
        files = outcome.transferred,
        changing = outcome.changing.len(),
        failed = outcome.failed.len(),
        elapsed_ms = transfer_started.elapsed().as_millis() as u64,
        "destination transfer phase complete"
    );

    // 4. Mirror: remove destination paths the source no longer has (deepest first).
    if sync.mirror {
        let extra_paths = diff.extra_paths_deepest_first();
        remove_paths_on_machine(
            dst_machine_id,
            dst_machine,
            dst_root,
            &extra_paths,
            cycle_id,
            timeout,
        )
        .context("failed to remove extra destination paths")?;
    }

    // 5. Content-equal files whose permission bits drifted: chmod in place.
    let mode_fixes: Vec<TransferModeSpec> = diff
        .mode_fixes
        .iter()
        .map(|entry| TransferModeSpec {
            rel_path: entry.rel_path.clone(),
            mode: entry.mode,
        })
        .collect();
    set_modes_on_machine(dst_machine_id, dst_machine, dst_root, &mode_fixes, timeout)
        .context("failed to repair destination file modes")?;

    set_dir_mtimes_on_machine(dst_machine_id, dst_machine, dst_root, &dirs, timeout)
        .context("failed to set destination directory mtimes")?;

    cleanup_tmp_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        cycle_id,
        sync.trash_keep_days,
        timeout,
    )
    .ok();

    // No end-of-cycle destination re-scan: every transferred file was verified
    // end-to-end at receipt (blake3 full hash checked before the atomic rename,
    // acked per file), removals ack per path, and untouched entries were
    // compared against the fresh destination scan taken above. Re-walking a
    // multi-terabyte destination tree here doubled the cycle cost for no
    // additional guarantee.
    outcome.into_result()
}

#[allow(clippy::too_many_arguments)]
fn sync_directory_event_paths_with_transfer(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine_id: &str,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    rel_paths: &[DiffPath],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }
    let timeout = transfer_timeout(sync);
    prepare_dir_on_machine(dst_machine_id, dst_machine, dst_root, None, None, timeout)?;
    let source_snapshot = snapshot_paths_on_machine(
        source_machine_id,
        source_machine,
        source_root,
        rel_paths,
        TransferSnapshotMode::Source,
        excludes,
        sync.checksum,
        timeout,
    )?;
    let dst_snapshot = snapshot_paths_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        rel_paths,
        TransferSnapshotMode::Destination,
        &[],
        sync.checksum,
        timeout,
    )?;
    let source_map = map_entry_refs(&source_snapshot);
    let dst_map = map_entry_refs(&dst_snapshot);

    let mut remove_paths: Vec<String> = source_snapshot
        .iter()
        .filter(|entry| {
            dst_map
                .get(entry.rel_path.as_str())
                .is_some_and(|existing| existing.file_type != entry.file_type)
        })
        .map(|entry| entry.rel_path.clone())
        .collect();

    if sync.mirror {
        remove_paths.extend(
            dst_map
                .keys()
                .filter(|rel| {
                    !source_map.contains_key(*rel) && !is_rel_excluded(Path::new(rel), excludes)
                })
                .map(|rel| rel.to_string()),
        );
        for path in rel_paths {
            let rel = path.rel.as_str();
            if !source_map.contains_key(rel) && !is_rel_excluded(Path::new(rel), excludes) {
                remove_paths.push(rel.to_string());
            }
        }
    }
    remove_paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
    remove_paths.dedup();
    remove_paths_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        &remove_paths,
        cycle_id,
        timeout,
    )
    .context("failed to remove changed destination paths")?;

    let mut dirs: Vec<TransferDirSpec> = source_snapshot
        .iter()
        .filter(|entry| entry.file_type == "dir")
        .map(|entry| TransferDirSpec {
            rel_path: entry.rel_path.clone(),
            mode: entry.mode,
            mtime_ns: entry.mtime_ns,
        })
        .collect();
    dirs.sort_by(|a, b| {
        path_depth(&a.rel_path)
            .cmp(&path_depth(&b.rel_path))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });
    prepare_dirs_on_machine(dst_machine_id, dst_machine, dst_root, &dirs, timeout)
        .context("failed to create changed destination directories")?;

    // Event paths carry positive change evidence: exact comparison, so a
    // same-size rewrite within the modify window still copies.
    let pending: Vec<(&SnapshotEntry, bool)> = source_snapshot
        .iter()
        .filter(|entry| entry.file_type == "file" || entry.file_type == "symlink")
        .filter_map(|entry| match dst_map.get(entry.rel_path.as_str()) {
            Some(existing) if entries_match_exact(entry, existing, sync) => None,
            Some(existing) => Some((entry, should_attempt_delta(entry, existing))),
            None => Some((entry, false)),
        })
        .collect();
    let mode_fixes: Vec<TransferModeSpec> = source_snapshot
        .iter()
        .filter_map(|entry| match dst_map.get(entry.rel_path.as_str()) {
            Some(existing)
                if entries_match_exact(entry, existing, sync)
                    && entry_mode_differs(entry, existing) =>
            {
                Some(TransferModeSpec {
                    rel_path: entry.rel_path.clone(),
                    mode: entry.mode,
                })
            }
            _ => None,
        })
        .collect();
    let outcome = push_entries_parallel(
        source_machine_id,
        source_machine,
        source_root,
        dst_machine,
        dst_root,
        destination_id,
        cycle_id,
        &pending,
        sync,
    )?;
    set_modes_on_machine(dst_machine_id, dst_machine, dst_root, &mode_fixes, timeout)
        .context("failed to repair destination file modes")?;
    info!(
        destination = destination_id,
        cycle_id,
        changed_paths = rel_paths.len(),
        dirs = dirs.len(),
        files = outcome.transferred,
        changing = outcome.changing.len(),
        failed = outcome.failed.len(),
        "destination realtime event transfer phase complete"
    );

    set_dir_mtimes_on_machine(dst_machine_id, dst_machine, dst_root, &dirs, timeout)
        .context("failed to set changed destination directory mtimes")?;

    cleanup_tmp_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        cycle_id,
        sync.trash_keep_days,
        timeout,
    )
    .ok();
    let actual = snapshot_paths_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        rel_paths,
        TransferSnapshotMode::Destination,
        &[],
        sync.checksum,
        timeout,
    )?;
    verify_snapshot_entries(
        &source_snapshot,
        &actual,
        &outcome.unverifiable_paths(),
        excludes,
        sync,
    )?;
    outcome.into_result()
}

fn sync_cycle_file_with_transfer(
    cfg: &AppConfig,
    state: &mut State,
    source: &SourceGroupConfig,
    cycle: &Cycle,
    source_info: &TransferPathInfo,
) -> Result<SyncCycleOutcome> {
    let source_machine_id = machine_id_or_local(&source.machine_id);
    let source_machine = find_machine(cfg, source_machine_id)
        .ok_or_else(|| anyhow!("unknown source machine: {source_machine_id}"))?;
    let mut targeted_indexes = Vec::new();
    for (dst_index, dst) in source
        .destinations
        .iter()
        .enumerate()
        .filter(|(_, d)| d.enabled)
    {
        if state.destination_target_cycle(&source.id, &dst.id)? == Some(cycle.id) {
            targeted_indexes.push(dst_index);
        }
    }
    let source_checksum = any_ready_destination_needs_checksum(cfg, source, &targeted_indexes);
    let mut source_snapshot = snapshot_on_machine(
        source_machine_id,
        &source_machine,
        &source_info.base,
        TransferSnapshotMode::Source,
        &source.excludes,
        source_checksum,
        ready_destination_timeout(cfg, source, &targeted_indexes)
            .max(snapshot_timeout_floor(source_checksum)),
    )?;
    source_snapshot.retain(|entry| entry.rel_path == source_info.name);

    let mut all_verified = true;
    let mut targeted_count = 0_usize;
    let mut blocked_count = 0_usize;
    let mut had_unblocked_failure = false;
    let mut progressed = false;

    state.mark_cycle_status(cycle.id, "syncing")?;
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if state.destination_target_cycle(&source.id, &dst.id)? != Some(cycle.id) {
            continue;
        }
        targeted_count += 1;
        // Paused by the user: hold the pending target (resume picks it up
        // exactly where the pause left off) without driving any work.
        if dst.paused {
            all_verified = false;
            blocked_count += 1;
            state.upsert_destination_status(&source.id, &dst.id, None, "yellow", "paused")?;
            continue;
        }
        let dst_machine_id = machine_id_or_local(&dst.machine_id);
        let Some(dst_machine) = find_machine(cfg, dst_machine_id) else {
            all_verified = false;
            had_unblocked_failure = true;
            state.upsert_destination_status(
                &source.id,
                &dst.id,
                None,
                "red",
                "unknown_destination_machine",
            )?;
            continue;
        };

        let result = sync_file_with_transfer(
            source_machine_id,
            &source_machine,
            &source_info.base,
            dst_machine_id,
            &dst_machine,
            &dst.path,
            &dst.id,
            cycle.id,
            &source_snapshot,
            &effective_sync_config(cfg, dst),
        );
        match result {
            Ok(()) => {
                progressed = true;
                state.clear_destination_issues(&source.id, &dst.id)?;
                state.upsert_destination_status(
                    &source.id,
                    &dst.id,
                    Some(cycle.id),
                    "green",
                    "verified",
                )?;
            }
            Err(err) => {
                all_verified = false;
                had_unblocked_failure = true;
                record_destination_failure(state, &source.id, &dst.id, cycle.id, &err)?;
            }
        }
    }

    if targeted_count == 0 || all_verified {
        state.mark_cycle_status(cycle.id, "verified")?;
    } else if blocked_count > 0 && !had_unblocked_failure {
        state.mark_cycle_status(cycle.id, "closed")?;
    } else {
        state.mark_cycle_status(cycle.id, "failed")?;
    }
    Ok(SyncCycleOutcome {
        progressed,
        blocked: blocked_count > 0,
    })
}

fn sync_file_with_transfer(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine_id: &str,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    sync: &NativeSyncConfig,
) -> Result<()> {
    let timeout = transfer_timeout(sync);
    prepare_dir_on_machine(dst_machine_id, dst_machine, dst_root, None, None, timeout)?;
    for entry in source_snapshot {
        let dst_snapshot = snapshot_on_machine(
            dst_machine_id,
            dst_machine,
            dst_root,
            TransferSnapshotMode::Destination,
            &[],
            sync.checksum,
            snapshot_timeout(sync),
        )?;
        let dst_map = map_entry_refs(&dst_snapshot);
        let needs_copy = match dst_map.get(entry.rel_path.as_str()) {
            Some(existing) => !entries_match(entry, existing, sync),
            None => true,
        };
        if needs_copy {
            let mut use_delta = false;
            if let Some(existing) = dst_map.get(entry.rel_path.as_str()) {
                if existing.file_type != entry.file_type {
                    remove_path_on_machine(
                        dst_machine_id,
                        dst_machine,
                        dst_root,
                        &entry.rel_path,
                        cycle_id,
                        timeout,
                    )?;
                } else {
                    use_delta = should_attempt_delta(entry, existing);
                }
            } else {
                use_delta = false;
            }
            let _transfer =
                progress::begin_transfer(destination_id, dst_root, entry.size.max(0) as u64);
            push_entry_between_machines(
                source_machine_id,
                source_machine,
                source_root,
                dst_machine,
                dst_root,
                destination_id,
                cycle_id,
                entry,
                use_delta,
                sync,
            )?;
        }
    }
    // Verify just the transferred file paths instead of re-walking the whole
    // destination directory.
    let rel_paths: Vec<DiffPath> = source_snapshot
        .iter()
        .map(|entry| DiffPath {
            rel: entry.rel_path.clone(),
            recursive: true,
        })
        .collect();
    let actual = snapshot_paths_on_machine(
        dst_machine_id,
        dst_machine,
        dst_root,
        &rel_paths,
        TransferSnapshotMode::Destination,
        &[],
        sync.checksum,
        timeout,
    )?;
    verify_snapshot_entries(source_snapshot, &actual, &BTreeSet::new(), &[], sync)?;
    Ok(())
}

fn path_info_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    path: &Path,
) -> Result<TransferPathInfo> {
    let req = TransferPathInfoRequest {
        path: path.to_path_buf(),
    };
    if machine_id == "local" {
        transfer_path_info(req)
    } else {
        remote_post_json(
            machine,
            "/api/transfer/path-info",
            &req,
            Duration::from_secs(DEFAULT_TRANSFER_TIMEOUT_SECS),
        )
    }
}

fn snapshot_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    mode: TransferSnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
    timeout: Duration,
) -> Result<Vec<SnapshotEntry>> {
    let req = TransferSnapshotRequest {
        root: root.to_path_buf(),
        mode,
        excludes: excludes.to_vec(),
        checksum,
        purpose: snapshot_purpose().to_string(),
        scope: cancel::current_target().unwrap_or_default(),
    };
    if machine_id == "local" {
        transfer_snapshot(req)
    } else {
        remote_snapshot_with_progress(machine, root, &req, timeout)
    }
}

/// The cancel kind a snapshot requested right now should register under on
/// the serving peer: compares tag their walks via the compare context.
fn snapshot_purpose() -> &'static str {
    if progress::in_compare_context() {
        cancel::KIND_COMPARE
    } else {
        cancel::KIND_SYNC
    }
}

fn snapshot_paths_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    paths: &[DiffPath],
    mode: TransferSnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
    timeout: Duration,
) -> Result<Vec<SnapshotEntry>> {
    let req = TransferSnapshotPathsRequest {
        root: root.to_path_buf(),
        mode,
        rel_paths: diff_path_rels(paths),
        non_recursive: paths
            .iter()
            .filter(|path| !path.recursive)
            .map(|path| path.rel.clone())
            .collect(),
        excludes: excludes.to_vec(),
        checksum,
        purpose: snapshot_purpose().to_string(),
        scope: cancel::current_target().unwrap_or_default(),
    };
    if machine_id == "local" {
        transfer_snapshot_paths(req)
    } else {
        remote_post_json(machine, "/api/transfer/snapshot-paths", &req, timeout)
    }
}

fn remote_snapshot_with_progress(
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    req: &TransferSnapshotRequest,
    timeout: Duration,
) -> Result<Vec<SnapshotEntry>> {
    let stop = Arc::new(AtomicBool::new(false));
    let poll_stop = Arc::clone(&stop);
    let poll_machine = machine.clone();
    let poll_root = root.to_path_buf();
    // Thread-locals do not cross spawn; carry the compare tag explicitly so the
    // mirrored remote progress lands in the right UI view.
    let compare_context = progress::in_compare_context();
    let poller = thread::spawn(move || {
        let _compare = compare_context.then(progress::enter_compare_context);
        let scan_progress = progress::start_scan(&poll_root);
        while !poll_stop.load(Ordering::Relaxed) {
            if let Ok(status) = remote_get_json::<PeerRuntimeStatus>(
                &poll_machine,
                "/api/runtime-status",
                Duration::from_secs(1),
            ) {
                if let Some(scan) = status.scan {
                    let current = if scan.current_path.is_empty() {
                        scan.root_path
                    } else {
                        scan.current_path
                    };
                    scan_progress.update(Path::new(&current), scan.entries_seen);
                }
            }
            thread::sleep(Duration::from_millis(250));
        }
    });
    let result = remote_snapshot_streamed(machine, req, timeout).or_else(|err| {
        // A peer without the streaming endpoint (mid-rollout mixed versions)
        // answers 404/405; everything else — including a status-line error
        // from a failed walk — must not trigger a second whole-tree walk.
        if error_is_missing_endpoint(&err) {
            remote_post_json(machine, "/api/transfer/snapshot", req, timeout)
        } else {
            Err(err)
        }
    });
    stop.store(true, Ordering::Relaxed);
    let _ = poller.join();
    result
}

fn error_is_missing_endpoint(err: &anyhow::Error) -> bool {
    let text = err.to_string();
    text.contains("non-200 response: HTTP/1.1 404")
        || text.contains("non-200 response: HTTP/1.1 405")
}

/// Fetch a whole-tree snapshot via the peer's streaming NDJSON endpoint,
/// parsing entries as they arrive instead of buffering ~100MB of JSON first.
/// The trailing [`SnapshotStreamStatus`] line decides success: streaming
/// starts before the peer's walk finishes, so transport-level 200 alone
/// proves nothing.
fn remote_snapshot_streamed(
    machine: &crate::core::config::MachineConfig,
    req: &TransferSnapshotRequest,
    timeout: Duration,
) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    let mut status: Option<SnapshotStreamStatus> = None;
    crate::core::machines::remote_post_ndjson(
        machine,
        "/api/transfer/snapshot-stream",
        req,
        timeout,
        &mut |line| {
            if line.is_empty() {
                return Ok(());
            }
            if let Ok(mark) = serde_json::from_slice::<SnapshotStreamStatus>(line) {
                status = Some(mark);
                return Ok(());
            }
            let entry: SnapshotEntry =
                serde_json::from_slice(line).context("failed to parse streamed snapshot entry")?;
            entries.push(entry);
            Ok(())
        },
    )?;
    match status {
        Some(mark) if mark.status == "ok" => Ok(entries),
        Some(mark) => bail!("peer snapshot walk failed: {}", mark.message),
        None => bail!("peer snapshot stream ended without a status line"),
    }
}

fn prepare_dir_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    rel_path: Option<&str>,
    mode: Option<u32>,
    timeout: Duration,
) -> Result<()> {
    let req = TransferPrepareDirRequest {
        root: root.to_path_buf(),
        rel_path: rel_path.map(ToString::to_string),
        mode,
    };
    let ack = if machine_id == "local" {
        transfer_prepare_dir(req)?
    } else {
        remote_post_json(machine, "/api/transfer/prepare-dir", &req, timeout)?
    };
    if !ack.ok {
        bail!("peer rejected prepare directory request");
    }
    Ok(())
}

fn remove_path_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    rel_path: &str,
    cycle_id: i64,
    timeout: Duration,
) -> Result<()> {
    let req = TransferRemovePathRequest {
        root: root.to_path_buf(),
        rel_path: rel_path.to_string(),
        cycle_id,
    };
    let ack = if machine_id == "local" {
        transfer_remove_path(req)?
    } else {
        remote_post_json(machine, "/api/transfer/remove-path", &req, timeout)?
    };
    if !ack.ok {
        bail!("peer rejected remove path request");
    }
    Ok(())
}

/// Maximum number of directory/path entries packed into a single bulk request.
const BULK_BATCH_SIZE: usize = 20_000;

fn prepare_dirs_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    dirs: &[TransferDirSpec],
    timeout: Duration,
) -> Result<()> {
    for chunk in dirs.chunks(BULK_BATCH_SIZE) {
        let req = TransferPrepareDirsRequest {
            root: root.to_path_buf(),
            dirs: chunk.to_vec(),
        };
        let ack = if machine_id == "local" {
            transfer_prepare_dirs(req)?
        } else {
            remote_post_json(machine, "/api/transfer/prepare-dirs", &req, timeout)?
        };
        if !ack.ok {
            bail!("peer rejected prepare directories request");
        }
    }
    Ok(())
}

fn set_modes_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    items: &[TransferModeSpec],
    timeout: Duration,
) -> Result<()> {
    for chunk in items.chunks(BULK_BATCH_SIZE) {
        let req = TransferSetModesRequest {
            root: root.to_path_buf(),
            items: chunk.to_vec(),
        };
        let ack = if machine_id == "local" {
            transfer_set_modes(req)?
        } else {
            remote_post_json(machine, "/api/transfer/set-modes", &req, timeout)?
        };
        if !ack.ok {
            bail!("peer rejected set modes request");
        }
    }
    Ok(())
}

fn set_dir_mtimes_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    dirs: &[TransferDirSpec],
    timeout: Duration,
) -> Result<()> {
    let mut dirs = dirs.to_vec();
    dirs.sort_by(|a, b| {
        path_depth(&b.rel_path)
            .cmp(&path_depth(&a.rel_path))
            .then_with(|| b.rel_path.cmp(&a.rel_path))
    });
    for chunk in dirs.chunks(BULK_BATCH_SIZE) {
        let req = TransferSetDirMtimesRequest {
            root: root.to_path_buf(),
            dirs: chunk.to_vec(),
        };
        let ack = if machine_id == "local" {
            transfer_set_dir_mtimes(req)?
        } else {
            remote_post_json(machine, "/api/transfer/set-dir-mtimes", &req, timeout)?
        };
        if !ack.ok {
            bail!("peer rejected set directory mtimes request");
        }
    }
    Ok(())
}

fn remove_paths_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    rel_paths: &[String],
    cycle_id: i64,
    timeout: Duration,
) -> Result<()> {
    for chunk in rel_paths.chunks(BULK_BATCH_SIZE) {
        let req = TransferRemovePathsRequest {
            root: root.to_path_buf(),
            rel_paths: chunk.to_vec(),
            cycle_id,
        };
        let ack = if machine_id == "local" {
            transfer_remove_paths(req)?
        } else {
            remote_post_json(machine, "/api/transfer/remove-paths", &req, timeout)?
        };
        if !ack.ok {
            bail!("peer rejected remove paths request");
        }
    }
    Ok(())
}

fn cleanup_tmp_on_machine(
    machine_id: &str,
    machine: &crate::core::config::MachineConfig,
    root: &Path,
    cycle_id: i64,
    trash_keep_days: u64,
    timeout: Duration,
) -> Result<()> {
    let req = TransferCleanupTmpRequest {
        root: root.to_path_buf(),
        cycle_id,
        trash_keep_days: Some(trash_keep_days),
    };
    if machine_id == "local" {
        transfer_cleanup_tmp(req)?;
    } else {
        let _: TransferAck = remote_post_json(machine, "/api/transfer/cleanup-tmp", &req, timeout)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_entry_between_machines(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    use_delta: bool,
    sync: &NativeSyncConfig,
) -> Result<()> {
    let req = TransferPushFileRequest {
        source_root: source_root.to_path_buf(),
        rel_path: entry.rel_path.clone(),
        entry: entry.clone(),
        destination: dst_machine.clone(),
        destination_root: dst_root.to_path_buf(),
        destination_id: destination_id.to_string(),
        cycle_id,
        transfer_timeout_secs: sync.transfer_timeout_secs.max(1),
        bwlimit_kbps: sync.bwlimit_kbps,
        use_delta,
    };
    let ack = if source_machine_id == "local" {
        transfer_push_file(req)?
    } else {
        remote_post_json(
            source_machine,
            "/api/transfer/push-file",
            &req,
            transfer_timeout(sync),
        )?
    };
    if !ack.ok {
        bail!("peer rejected file push request");
    }
    Ok(())
}

fn resolve_parallelism(configured: usize, work_items: usize) -> usize {
    let requested = if configured == 0 {
        DEFAULT_MAX_PARALLEL_TRANSFERS
    } else {
        configured
    };
    requested.clamp(1, work_items.max(1))
}

const TRANSFER_RETRY_ATTEMPTS: u32 = 3;

/// Errors whose retry can never succeed within this cycle: the source mutated
/// under us, so re-reading just races again. The caller records them as a
/// tolerated `source_changing` issue instead of retrying or failing red.
fn transfer_error_is_source_changing(err: &anyhow::Error) -> bool {
    !source_changed_paths(err).is_empty()
}

/// Connection-level failures: the destination (or the pushing source machine)
/// is unreachable, so every remaining file would fail the same way. The worker
/// pool aborts immediately instead of burning retries per file.
fn transfer_error_is_fatal(err: &anyhow::Error) -> bool {
    // A user cancellation dooms every remaining file the same way a dead
    // connection does: abort the pool instead of failing files one by one.
    if cancel::error_is_cancelled(err) {
        return true;
    }
    err.chain().any(|cause| {
        if let Some(io_err) = cause.downcast_ref::<io::Error>() {
            return matches!(
                io_err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::AddrNotAvailable
            );
        }
        let text = cause.to_string();
        text.contains("peer closed connection") || text.contains("HTTP request failed")
    })
}

/// Run a single-file transfer, retrying transient failures with exponential
/// backoff before giving up. Each transfer path is idempotent on retry (chunked
/// resumes from the receiver's offset, put-file/delta overwrite the temp file),
/// so a retry cannot corrupt a partial result. Source-changing errors are
/// terminal for this cycle and returned without retry.
fn with_transfer_retry<F>(label: &str, mut attempt_fn: F) -> Result<()>
where
    F: FnMut() -> Result<()>,
{
    let mut attempt = 0_u32;
    loop {
        attempt += 1;
        match attempt_fn() {
            Ok(()) => return Ok(()),
            Err(err)
                if attempt < TRANSFER_RETRY_ATTEMPTS
                    && !transfer_error_is_source_changing(&err)
                    && !cancel::error_is_cancelled(&err) =>
            {
                warn!(
                    rel_path = label,
                    attempt,
                    error = %err,
                    "transfer attempt failed; retrying after backoff"
                );
                thread::sleep(Duration::from_millis(200_u64 << (attempt - 1)));
            }
            Err(err) => return Err(err),
        }
    }
}

/// A completed transfer/copy phase: how many entries transferred, which source
/// paths changed mid-copy (tolerated; the destination goes yellow and the next
/// cycle converges them), and which entries failed for per-file reasons (the
/// rest of the batch still transferred, so progress is preserved).
#[derive(Debug)]
struct TransferOutcome {
    transferred: usize,
    changing: BTreeSet<String>,
    failed: Vec<(String, anyhow::Error)>,
}

impl TransferOutcome {
    /// Paths that must be excluded from post-copy verification: they are known
    /// not to match the source snapshot (changed mid-copy or failed to copy).
    fn unverifiable_paths(&self) -> BTreeSet<String> {
        let mut paths = self.changing.clone();
        paths.extend(self.failed.iter().map(|(path, _)| path.clone()));
        paths
    }

    /// The error this outcome implies, if any: per-file failures dominate
    /// (red), otherwise tolerated source changes (yellow via
    /// `source_changing_error`), otherwise success.
    fn into_result(self) -> Result<()> {
        let failed_count = self.failed.len();
        if let Some((path, err)) = self.failed.into_iter().next() {
            return Err(err.context(format!(
                "{failed_count} file transfer(s) failed (first: {path})"
            )));
        }
        if !self.changing.is_empty() {
            return Err(source_changing_error(&self.changing));
        }
        Ok(())
    }
}

/// Per-file failures tolerated before the worker pool gives up: a broken
/// destination fails every file the same way, and there is no point burning
/// retries on hundreds of thousands of doomed transfers.
const MAX_PER_FILE_TRANSFER_FAILURES: usize = 20;

/// A unit of work for the push pool: one entry, or one batch of small files
/// delivered in a single request.
enum PushWork<'a> {
    Single(&'a SnapshotEntry, bool),
    SmallBatch(Vec<&'a SnapshotEntry>),
}

/// Group small non-delta files into batch work items (bounded by
/// [`SMALL_BATCH_MAX_FILES`]/[`SMALL_BATCH_MAX_BYTES`]); everything else
/// stays a per-entry item. A batch of one gains nothing over put-file, so it
/// degenerates to a Single.
fn plan_push_work<'a>(entries: &[(&'a SnapshotEntry, bool)]) -> Vec<PushWork<'a>> {
    fn flush<'a>(
        work: &mut Vec<PushWork<'a>>,
        batch: &mut Vec<&'a SnapshotEntry>,
        batch_bytes: &mut usize,
    ) {
        match batch.len() {
            0 => {}
            1 => work.push(PushWork::Single(batch[0], false)),
            _ => work.push(PushWork::SmallBatch(std::mem::take(batch))),
        }
        batch.clear();
        *batch_bytes = 0;
    }
    let mut work = Vec::new();
    let mut batch: Vec<&SnapshotEntry> = Vec::new();
    let mut batch_bytes = 0_usize;
    for (entry, use_delta) in entries {
        let size = entry.size.max(0) as u64;
        let small = entry.file_type == "file" && !use_delta && size <= SMALL_BATCH_FILE_MAX;
        if !small {
            work.push(PushWork::Single(entry, *use_delta));
            continue;
        }
        if batch.len() >= SMALL_BATCH_MAX_FILES
            || batch_bytes + size as usize > SMALL_BATCH_MAX_BYTES
        {
            flush(&mut work, &mut batch, &mut batch_bytes);
        }
        batch.push(entry);
        batch_bytes += size as usize;
    }
    flush(&mut work, &mut batch, &mut batch_bytes);
    work
}

/// Deliver one planned small-file batch, delegating to the source machine
/// when the files live there (mirrors [`push_entry_between_machines`]).
#[allow(clippy::too_many_arguments)]
fn push_files_batch_between_machines(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    batch: &[&SnapshotEntry],
    sync: &NativeSyncConfig,
) -> Result<TransferBatchOutcome> {
    let req = TransferPushFilesBatchRequest {
        source_root: source_root.to_path_buf(),
        entries: batch.iter().map(|entry| (*entry).clone()).collect(),
        destination: dst_machine.clone(),
        destination_root: dst_root.to_path_buf(),
        destination_id: destination_id.to_string(),
        cycle_id,
        transfer_timeout_secs: sync.transfer_timeout_secs.max(1),
        bwlimit_kbps: sync.bwlimit_kbps,
    };
    if source_machine_id == "local" {
        transfer_push_files_batch(req)
    } else {
        match remote_post_json(
            source_machine,
            "/api/transfer/push-files-batch",
            &req,
            transfer_timeout(sync),
        ) {
            Ok(outcome) => Ok(outcome),
            // Old source machine without the batch endpoint: fall back to
            // per-file delegated pushes.
            Err(err) if error_is_missing_endpoint(&err) => {
                let mut outcome = TransferBatchOutcome::default();
                for entry in batch {
                    match push_entry_between_machines(
                        source_machine_id,
                        source_machine,
                        source_root,
                        dst_machine,
                        dst_root,
                        destination_id,
                        cycle_id,
                        entry,
                        false,
                        sync,
                    ) {
                        Ok(()) => outcome.sent += 1,
                        Err(err) if transfer_error_is_source_changing(&err) => {
                            outcome.changing.push(entry.rel_path.clone());
                        }
                        Err(err) => outcome.failed.push(BatchFileFailure {
                            rel_path: entry.rel_path.clone(),
                            error: format!("{err:#}"),
                        }),
                    }
                }
                Ok(outcome)
            }
            Err(err) => Err(err),
        }
    }
}

/// Transfer the given entries to the destination using a bounded worker pool.
/// Source-changing failures are collected (not fatal); other per-file failures
/// are collected up to [`MAX_PER_FILE_TRANSFER_FAILURES`]; connection-level
/// failures abort the pool immediately and are returned as the error.
#[allow(clippy::too_many_arguments)]
fn push_entries_parallel(
    source_machine_id: &str,
    source_machine: &crate::core::config::MachineConfig,
    source_root: &Path,
    dst_machine: &crate::core::config::MachineConfig,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entries: &[(&SnapshotEntry, bool)],
    sync: &NativeSyncConfig,
) -> Result<TransferOutcome> {
    set_sync_phase("transferring");
    if entries.is_empty() {
        return Ok(TransferOutcome {
            transferred: 0,
            changing: BTreeSet::new(),
            failed: Vec::new(),
        });
    }
    let total_bytes: u64 = entries
        .iter()
        .filter(|(entry, _)| entry.file_type == "file")
        .map(|(entry, _)| entry.size.max(0) as u64)
        .sum();
    let _transfer = progress::begin_transfer(destination_id, dst_root, total_bytes);
    // Small files ride shared batch requests (one round-trip + one receiver
    // durability barrier for up to 200 files); everything else stays per-file.
    let work = plan_push_work(entries);
    let workers = resolve_parallelism(sync.max_parallel_transfers, work.len());
    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let fatal_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let changing: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let failed: Mutex<Vec<(String, anyhow::Error)>> = Mutex::new(Vec::new());
    // Push a failure under the shared cap; returns true when the cap was hit
    // and the pool should abort with the aggregated fatal error.
    let push_failed = |rel_path: String, err: anyhow::Error| -> bool {
        let mut list = failed.lock().unwrap_or_else(|e| e.into_inner());
        list.push((rel_path, err));
        if list.len() >= MAX_PER_FILE_TRANSFER_FAILURES {
            let (path, err) = list.pop().expect("failure list cannot be empty at its cap");
            let mut slot = fatal_error.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(err.context(format!(
                    "giving up after {MAX_PER_FILE_TRANSFER_FAILURES} \
                     file transfer failures (last: {path})"
                )));
            }
            return true;
        }
        false
    };
    // Thread-locals do not cross spawn; carry the cancel token explicitly so
    // a cancel request stops the workers, not just the coordinating thread.
    let cancel_token = cancel::current_token();

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let _cancel = cancel::enter(cancel_token.clone());
                loop {
                    if fatal_error
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .is_some()
                    {
                        break;
                    }
                    if let Err(err) = cancel::check() {
                        let mut slot = fatal_error.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_none() {
                            *slot = Some(err);
                        }
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= work.len() {
                        break;
                    }
                    match &work[idx] {
                        PushWork::Single(entry, use_delta) => {
                            match with_transfer_retry(&entry.rel_path, || {
                                push_entry_between_machines(
                                    source_machine_id,
                                    source_machine,
                                    source_root,
                                    dst_machine,
                                    dst_root,
                                    destination_id,
                                    cycle_id,
                                    entry,
                                    *use_delta,
                                    sync,
                                )
                                .with_context(|| format!("failed to transfer {}", entry.rel_path))
                            }) {
                                Ok(()) => {
                                    done.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(err) if transfer_error_is_source_changing(&err) => {
                                    changing
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .insert(entry.rel_path.clone());
                                }
                                Err(err) if transfer_error_is_fatal(&err) => {
                                    let mut slot =
                                        fatal_error.lock().unwrap_or_else(|e| e.into_inner());
                                    if slot.is_none() {
                                        *slot = Some(err);
                                    }
                                    break;
                                }
                                Err(err) => {
                                    warn!(
                                        rel_path = entry.rel_path,
                                        error = %err,
                                        "file transfer failed; continuing with remaining files"
                                    );
                                    if push_failed(entry.rel_path.clone(), err) {
                                        break;
                                    }
                                }
                            }
                        }
                        PushWork::SmallBatch(batch) => {
                            let label = format!(
                                "batch of {} small files (first: {})",
                                batch.len(),
                                batch[0].rel_path
                            );
                            let mut batch_outcome: Option<TransferBatchOutcome> = None;
                            let result = with_transfer_retry(&label, || {
                                batch_outcome = Some(
                                    push_files_batch_between_machines(
                                        source_machine_id,
                                        source_machine,
                                        source_root,
                                        dst_machine,
                                        dst_root,
                                        destination_id,
                                        cycle_id,
                                        batch,
                                        sync,
                                    )
                                    .with_context(|| format!("failed to transfer {label}"))?,
                                );
                                Ok(())
                            });
                            match result {
                                Ok(()) => {
                                    let outcome = batch_outcome
                                        .expect("batch outcome must be set on success");
                                    done.fetch_add(outcome.sent, Ordering::Relaxed);
                                    if !outcome.changing.is_empty() {
                                        changing
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .extend(outcome.changing);
                                    }
                                    let mut abort = false;
                                    for failure in outcome.failed {
                                        warn!(
                                            rel_path = failure.rel_path,
                                            error = failure.error,
                                            "file transfer failed; continuing with remaining files"
                                        );
                                        if push_failed(failure.rel_path, anyhow!(failure.error)) {
                                            abort = true;
                                            break;
                                        }
                                    }
                                    if abort {
                                        break;
                                    }
                                }
                                Err(err) if transfer_error_is_fatal(&err) => {
                                    let mut slot =
                                        fatal_error.lock().unwrap_or_else(|e| e.into_inner());
                                    if slot.is_none() {
                                        *slot = Some(err);
                                    }
                                    break;
                                }
                                Err(err) => {
                                    warn!(
                                        batch = label,
                                        error = %err,
                                        "batch transfer failed; continuing with remaining work"
                                    );
                                    if push_failed(label, err) {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    if let Some(err) = fatal_error
        .into_inner()
        .unwrap_or_else(|err| err.into_inner())
    {
        return Err(err);
    }
    let transferred = done.load(Ordering::Relaxed) as usize;
    cancel::add_synced_files(transferred as u64);
    sync_plan_add_done(transferred as u64);
    let failed = failed.into_inner().unwrap_or_else(|err| err.into_inner());
    cancel::add_failed_files(failed.len() as u64);
    Ok(TransferOutcome {
        transferred,
        changing: changing.into_inner().unwrap_or_else(|err| err.into_inner()),
        failed,
    })
}

/// Copy local file/symlink entries to the destination using a bounded worker
/// pool. Source-changing failures are collected (tolerated); other per-file
/// failures are collected up to [`MAX_PER_FILE_TRANSFER_FAILURES`] so one bad
/// file does not discard the progress of the rest of the batch. An aggregate
/// transfer meter must already be active.
///
/// Parallelism is media-aware: parallel small-file writes pay off on flash
/// but thrash rotational heads, so a destination detected as HDD runs the
/// pool with one worker. Undetectable media keep the configured parallel
/// default (local copy batches always ran parallel before detection existed).
fn copy_entries_parallel(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entries: &[&SnapshotEntry],
    sync: &NativeSyncConfig,
) -> Result<TransferOutcome> {
    if entries.is_empty() {
        return Ok(TransferOutcome {
            transferred: 0,
            changing: BTreeSet::new(),
            failed: Vec::new(),
        });
    }
    let workers = if storage::path_is_rotational(dst_root) == Some(true) {
        1
    } else {
        resolve_parallelism(sync.max_parallel_transfers, entries.len())
    };
    let next = AtomicUsize::new(0);
    let done = AtomicU64::new(0);
    let fatal_error: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let changing: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let failed: Mutex<Vec<(String, anyhow::Error)>> = Mutex::new(Vec::new());
    let cancel_token = cancel::current_token();

    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                let _cancel = cancel::enter(cancel_token.clone());
                loop {
                    if fatal_error
                        .lock()
                        .unwrap_or_else(|err| err.into_inner())
                        .is_some()
                    {
                        break;
                    }
                    if let Err(err) = cancel::check() {
                        let mut slot = fatal_error.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_none() {
                            *slot = Some(err);
                        }
                        break;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed);
                    if idx >= entries.len() {
                        break;
                    }
                    let entry = entries[idx];
                    match copy_entry(src_root, dst_root, destination_id, cycle_id, entry)
                        .with_context(|| format!("failed to copy {}", entry.rel_path))
                    {
                        Ok(()) => {
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(err) => {
                            let paths = source_changed_paths(&err);
                            if !paths.is_empty() {
                                changing
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .extend(paths);
                                continue;
                            }
                            warn!(
                                rel_path = entry.rel_path,
                                error = %err,
                                "file copy failed; continuing with remaining files"
                            );
                            let mut list = failed.lock().unwrap_or_else(|e| e.into_inner());
                            list.push((entry.rel_path.clone(), err));
                            if list.len() >= MAX_PER_FILE_TRANSFER_FAILURES {
                                let (path, err) =
                                    list.pop().expect("failure list cannot be empty at its cap");
                                let mut slot =
                                    fatal_error.lock().unwrap_or_else(|e| e.into_inner());
                                if slot.is_none() {
                                    *slot = Some(err.context(format!(
                                        "giving up after {MAX_PER_FILE_TRANSFER_FAILURES} \
                                         file copy failures (last: {path})"
                                    )));
                                }
                                break;
                            }
                        }
                    }
                }
            });
        }
    });

    if let Some(err) = fatal_error
        .into_inner()
        .unwrap_or_else(|err| err.into_inner())
    {
        return Err(err);
    }
    let transferred = done.load(Ordering::Relaxed) as usize;
    cancel::add_synced_files(transferred as u64);
    sync_plan_add_done(transferred as u64);
    let failed = failed.into_inner().unwrap_or_else(|err| err.into_inner());
    cancel::add_failed_files(failed.len() as u64);
    Ok(TransferOutcome {
        transferred,
        changing: changing.into_inner().unwrap_or_else(|err| err.into_inner()),
        failed,
    })
}

fn verify_snapshot_entries(
    expected: &[SnapshotEntry],
    actual: &[SnapshotEntry],
    ignored_paths: &BTreeSet<String>,
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    let expected = map_entry_refs(expected);
    let actual = map_entry_refs(actual);
    for (rel, want) in &expected {
        if ignored_paths.contains(*rel) {
            continue;
        }
        match actual.get(rel) {
            // Exact: the writes above stamped the destination with the
            // source's own mtime, so anything coarser hides real drift.
            Some(got) if entries_match_exact(want, got, sync) => {}
            Some(_) => bail!("destination mismatch at {rel}"),
            None => bail!("destination missing {rel}"),
        }
    }
    if sync.mirror {
        for rel in actual.keys() {
            if is_rel_excluded(Path::new(rel), excludes) || ignored_paths.contains(*rel) {
                continue;
            }
            if !expected.contains_key(rel) {
                bail!("destination has extra path {rel}");
            }
        }
    }
    Ok(())
}

fn cross_platform_file_name(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim_end_matches(|ch| ch == '/' || ch == '\\');
    let leaf = trimmed
        .rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or(trimmed);
    if leaf.ends_with(':') {
        return None;
    }
    if leaf.is_empty() {
        None
    } else {
        Some(leaf.to_string())
    }
}

fn join_machine_path(
    base: &Path,
    leaf: &str,
    machine: &crate::core::config::MachineConfig,
) -> PathBuf {
    let raw = base.to_string_lossy();
    let trimmed = raw.trim_end_matches(|ch| ch == '/' || ch == '\\');
    let sep = if machine.os.eq_ignore_ascii_case("windows") {
        "\\"
    } else {
        "/"
    };
    if trimmed.is_empty() {
        return PathBuf::from(format!("{sep}{leaf}"));
    }
    PathBuf::from(format!("{trimmed}{sep}{leaf}"))
}

fn destination_root_for_source(
    source: &SourceGroupConfig,
    source_info: &TransferPathInfo,
    dst_path: &Path,
    dst_machine: &crate::core::config::MachineConfig,
) -> PathBuf {
    if source.add_directory {
        join_machine_path(dst_path, &source_info.name, dst_machine)
    } else {
        dst_path.to_path_buf()
    }
}

#[derive(Debug, Clone)]
struct SourceReadView {
    endpoint: SourceEndpoint,
    zfs_snapshot: Option<ZfsSnapshot>,
}

impl SourceReadView {
    fn prepare(
        source: &SourceGroupConfig,
        live_endpoint: &SourceEndpoint,
        cycle_id: i64,
        cycle_starts_epoch: Option<i64>,
    ) -> Result<Self> {
        match source.snapshot.backend {
            SnapshotBackend::Manifest => Ok(Self {
                endpoint: live_endpoint.clone(),
                zfs_snapshot: None,
            }),
            SnapshotBackend::Auto | SnapshotBackend::Zfs => {
                match ZfsSnapshot::create(source, cycle_id, cycle_starts_epoch) {
                    Ok(snapshot) => {
                        let endpoint = match live_endpoint {
                            SourceEndpoint::Dir { add_directory, .. } => SourceEndpoint::Dir {
                                root: snapshot.source_path.clone(),
                                add_directory: *add_directory,
                            },
                            SourceEndpoint::File { .. } => SourceEndpoint::File {
                                path: snapshot.source_path.clone(),
                            },
                        };
                        info!(
                            source = source.id,
                            snapshot = snapshot.full_name,
                            path = %snapshot.source_path.display(),
                            "using zfs snapshot source view"
                        );
                        Ok(Self {
                            endpoint,
                            zfs_snapshot: Some(snapshot),
                        })
                    }
                    Err(err) if source.snapshot.backend == SnapshotBackend::Auto => {
                        warn!(
                            source = source.id,
                            error = %err,
                            "zfs snapshot unavailable; falling back to manifest source view"
                        );
                        Ok(Self {
                            endpoint: live_endpoint.clone(),
                            zfs_snapshot: None,
                        })
                    }
                    Err(err) => Err(err),
                }
            }
        }
    }

    fn cleanup(&self, source: &SourceGroupConfig, referenced: &[String]) {
        if let Some(snapshot) = &self.zfs_snapshot {
            if let Err(err) = cleanup_zfs_snapshots(source, snapshot, referenced) {
                warn!(source = source.id, error = %err, "zfs snapshot cleanup failed");
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ZfsSnapshot {
    dataset: String,
    full_name: String,
    source_path: PathBuf,
    /// Live filesystem root of the source within the dataset
    /// (`mountpoint`/`path_in_dataset`). `zfs diff` reports paths relative to
    /// this, so it is used to map diff output back to source-relative paths.
    source_live_root: PathBuf,
}

impl ZfsSnapshot {
    fn create(
        source: &SourceGroupConfig,
        cycle_id: i64,
        cycle_starts_epoch: Option<i64>,
    ) -> Result<Self> {
        let dataset = resolve_zfs_dataset(source)?;
        let snapshot_id = format!(
            "{}_{}_{:012}",
            source.snapshot.prefix,
            sanitize_snapshot_component(&source.id),
            cycle_id
        );
        let full_name = format!("{}@{}", dataset.name, snapshot_id);
        ensure_zfs_snapshot(&full_name, cycle_starts_epoch)?;
        let source_path = dataset
            .mountpoint
            .join(".zfs")
            .join("snapshot")
            .join(&snapshot_id)
            .join(&dataset.path_in_dataset);
        if !source_path.exists() {
            bail!(
                "zfs snapshot path is not visible: {}. Ensure snapdir=visible or use manifest backend",
                source_path.display()
            );
        }
        let source_live_root = if dataset.path_in_dataset.as_os_str().is_empty() {
            dataset.mountpoint.clone()
        } else {
            dataset.mountpoint.join(&dataset.path_in_dataset)
        };
        Ok(Self {
            dataset: dataset.name,
            full_name,
            source_path,
            source_live_root,
        })
    }
}

/// Relative paths (under the source root) that changed between two snapshots,
/// computed with `zfs diff`. Returns `None` when a reliable diff cannot be
/// produced (base snapshot missing, command failed) so the caller falls back to
/// a full reconcile. `zfs diff` is authoritative, so a successful diff is a
/// complete list of changed/added/removed/renamed paths.
/// A Phase label for a running `zfs diff` that names the cycle span it covers
/// (parsed from the snapshot names, `…_000000000057` → 57). A diff's cost is
/// proportional to how many cycles separate the base from the target, so
/// surfacing "zfs diff 57→85" makes a slow catch-up self-explaining instead of
/// looking frozen. Falls back to plain "zfs diff" when the names don't parse.
fn zfs_diff_phase_label(base_full_name: &str, new_full_name: &str) -> String {
    let cycle_of = |name: &str| -> Option<i64> {
        name.rsplit('_')
            .next()
            .and_then(|tail| tail.parse::<i64>().ok())
    };
    match (cycle_of(base_full_name), cycle_of(new_full_name)) {
        (Some(base), Some(to)) if base != to => format!("zfs diff {base}→{to}"),
        _ => "zfs diff".to_string(),
    }
}

fn zfs_diff_changed_paths(
    base_full_name: &str,
    new_full_name: &str,
    source_live_root: &Path,
) -> Option<Vec<DiffPath>> {
    set_sync_phase(&zfs_diff_phase_label(base_full_name, new_full_name));
    if base_full_name == new_full_name {
        return Some(Vec::new());
    }
    let base_exists = Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", base_full_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !base_exists {
        return None;
    }
    let output = Command::new("zfs")
        .args(["diff", "-H", base_full_name, new_full_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(parse_zfs_diff(&text, source_live_root))
}

/// One changed path from `zfs diff`, plus whether per-path snapshotting must
/// recurse below it when it is a directory. `M` on a directory means only the
/// directory entry itself changed (every changed child reports its own diff
/// line), so M-dirs skip the subtree walk — recursing them turned "one file
/// added to a big directory" into a walk of that whole directory. Created,
/// deleted, and renamed directories carry their subtree implicitly (rename
/// emits a single `R` line for the dir) and must recurse.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffPath {
    rel: String,
    recursive: bool,
}

/// Wrap plain relative paths (watcher events, wire requests without recursion
/// info) as fully recursive diff paths — the conservative, always-correct
/// reading.
fn diff_paths_all_recursive(rel_paths: &[String]) -> Vec<DiffPath> {
    rel_paths
        .iter()
        .map(|rel| DiffPath {
            rel: rel.clone(),
            recursive: true,
        })
        .collect()
}

/// The relative paths of `paths`, for logging, wire requests and touched sets.
fn diff_path_rels(paths: &[DiffPath]) -> Vec<String> {
    paths.iter().map(|path| path.rel.clone()).collect()
}

/// Merge both sides' diff paths into one union (recursion flags OR-merged on
/// collisions) and extract the destination-only subset. Paths only the
/// destination touched since its baseline are hard evidence of a dst-side
/// write; callers hash them on both sides regardless of the checksum setting
/// so a size+mtime-restoring rewrite cannot clear itself.
fn diff_union_and_dst_only(
    src_changed: Vec<DiffPath>,
    dst_changed: Vec<DiffPath>,
) -> (Vec<DiffPath>, Vec<DiffPath>) {
    let mut union: BTreeMap<String, bool> = src_changed
        .into_iter()
        .map(|path| (path.rel, path.recursive))
        .collect();
    let src_rels: BTreeSet<String> = union.keys().cloned().collect();
    let mut dst_only = Vec::new();
    for path in dst_changed {
        if !src_rels.contains(&path.rel) {
            dst_only.push(path.clone());
        }
        let slot = union.entry(path.rel).or_insert(path.recursive);
        *slot |= path.recursive;
    }
    let union = union
        .into_iter()
        .map(|(rel, recursive)| DiffPath { rel, recursive })
        .collect();
    (union, dst_only)
}

/// Parse `zfs diff -H` output into source-relative paths. Each line is
/// `<change>\t<path>` (or `R\t<old>\t<new>` for renames); paths are absolute
/// under the dataset mountpoint and octal-escaped. A path reported under
/// multiple change kinds keeps the stronger (recursive) reading.
fn parse_zfs_diff(output: &str, source_live_root: &Path) -> Vec<DiffPath> {
    let mut paths: BTreeMap<String, bool> = BTreeMap::new();
    for line in output.lines() {
        let mut fields = line.split('\t');
        let Some(change) = fields.next() else {
            continue;
        };
        if !matches!(change, "-" | "+" | "M" | "R") {
            continue;
        }
        let recursive = change != "M";
        for raw in fields {
            let abs = unescape_zfs_path(raw);
            if let Ok(rel) = Path::new(&abs).strip_prefix(source_live_root) {
                if rel.as_os_str().is_empty() {
                    continue;
                }
                if let Ok(rel_str) = rel_to_string(rel) {
                    // Never let auto_sync's own trash/tmp/probe into the
                    // union: mirror deletes rename into the trash, so every
                    // destination-side diff after a delete names it.
                    if rel_str_is_internal(&rel_str) {
                        continue;
                    }
                    let slot = paths.entry(rel_str).or_insert(recursive);
                    *slot |= recursive;
                }
            }
        }
    }
    paths
        .into_iter()
        .map(|(rel, recursive)| DiffPath { rel, recursive })
        .collect()
}

/// `zfs diff` escapes bytes outside the printable ASCII range as `\NNN` (three
/// octal digits, per OpenZFS `stream_bytes`). Decode them back to raw bytes.
fn unescape_zfs_path(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && (b'0'..=b'7').contains(&bytes[i + 1])
            && (b'0'..=b'7').contains(&bytes[i + 2])
            && (b'0'..=b'7').contains(&bytes[i + 3])
        {
            let value =
                (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(value);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[derive(Debug, Clone)]
struct ZfsDataset {
    name: String,
    mountpoint: PathBuf,
    path_in_dataset: PathBuf,
}

fn resolve_zfs_dataset(source: &SourceGroupConfig) -> Result<ZfsDataset> {
    if let Some(dataset) = &source.snapshot.dataset {
        let mountpoint = zfs_mountpoint(dataset)?;
        let path_in_dataset = source
            .snapshot
            .path_in_dataset
            .clone()
            .unwrap_or_else(|| path_in_dataset(&source.src, &mountpoint).unwrap_or_default());
        let live_root = if path_in_dataset.as_os_str().is_empty() {
            mountpoint.clone()
        } else {
            mountpoint.join(&path_in_dataset)
        };
        ensure_no_nested_mounts(dataset, &live_root)?;
        return Ok(ZfsDataset {
            name: dataset.clone(),
            mountpoint,
            path_in_dataset,
        });
    }

    let source_path = source
        .src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize source {}", source.src.display()))?;
    let mut best: Option<(String, PathBuf)> = None;
    for (name, mountpoint) in zfs_filesystems()? {
        if source_path.starts_with(&mountpoint) {
            let replace = best
                .as_ref()
                .map(|(_, current)| mountpoint.components().count() > current.components().count())
                .unwrap_or(true);
            if replace {
                best = Some((name, mountpoint));
            }
        }
    }
    let Some((name, mountpoint)) = best else {
        bail!("source is not on a zfs dataset: {}", source_path.display());
    };
    ensure_no_nested_mounts(&name, &source_path)?;
    let path_in_dataset = path_in_dataset(&source_path, &mountpoint)?;
    Ok(ZfsDataset {
        name,
        mountpoint,
        path_in_dataset,
    })
}

fn path_in_dataset(source_path: &Path, mountpoint: &Path) -> Result<PathBuf> {
    let rel = source_path
        .strip_prefix(mountpoint)
        .with_context(|| format!("source is not below mountpoint {}", mountpoint.display()))?;
    if rel.as_os_str().is_empty() {
        Ok(PathBuf::new())
    } else {
        Ok(rel.to_path_buf())
    }
}

fn zfs_filesystems() -> Result<Vec<(String, PathBuf)>> {
    let output = command_stdout(Command::new("zfs").args([
        "list",
        "-H",
        "-t",
        "filesystem",
        "-o",
        "name,mountpoint",
    ]))?;
    let mut filesystems = Vec::new();
    for line in output.lines() {
        // `-H` output is TAB-separated; splitting on whitespace truncated
        // mountpoints containing spaces (and the truncation could then match
        // an unrelated real directory).
        let mut parts = line.split('\t');
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(mountpoint) = parts.next() else {
            continue;
        };
        if mountpoint == "-" || mountpoint.is_empty() {
            continue;
        }
        filesystems.push((name.to_string(), PathBuf::from(mountpoint)));
    }
    Ok(filesystems)
}

fn zfs_mountpoint(dataset: &str) -> Result<PathBuf> {
    let output =
        command_stdout(Command::new("zfs").args(["list", "-H", "-o", "mountpoint", dataset]))?;
    let mountpoint = output.trim();
    if mountpoint.is_empty() || mountpoint == "-" {
        bail!("zfs dataset has no mounted mountpoint: {dataset}");
    }
    Ok(PathBuf::from(mountpoint))
}

/// Ensure `full_name` exists, guarding snapshot IDENTITY: names embed only
/// (prefix, source/pair, cycle id), and cycle ids restart from 1 when the
/// state database is reset — a leftover same-name snapshot from an older
/// database generation would otherwise be silently adopted as this cycle's
/// read view, rolling the destination back to stale data under a green
/// status. A snapshot created before `not_created_before` (unix seconds,
/// minus clock slack) is destroyed and recreated; `None` keeps the plain
/// reuse semantics (same-cycle retry).
fn ensure_zfs_snapshot(full_name: &str, not_created_before: Option<i64>) -> Result<()> {
    if let Ok(raw) = command_stdout(
        Command::new("zfs").args(["get", "-Hp", "-o", "value", "creation", full_name]),
    ) {
        let created = raw.trim().parse::<i64>().unwrap_or(0);
        // 5 minutes of slack: the cycle row and the snapshot are stamped by
        // clocks that may disagree slightly.
        let stale = not_created_before.is_some_and(|bound| created < bound - 300);
        if !stale {
            return Ok(());
        }
        warn!(
            snapshot = full_name,
            created,
            "same-name snapshot predates its cycle (older database generation?); recreating"
        );
        let status = Command::new("zfs")
            .args(["destroy", full_name])
            .status()
            .context("failed to execute zfs destroy")?;
        if !status.success() {
            bail!("zfs destroy failed for stale same-name snapshot {full_name}");
        }
    }
    let status = Command::new("zfs")
        .args(["snapshot", full_name])
        .status()
        .context("failed to execute zfs snapshot")?;
    if !status.success() {
        bail!("zfs snapshot failed for {full_name}");
    }
    Ok(())
}

fn cleanup_zfs_snapshots(
    source: &SourceGroupConfig,
    latest: &ZfsSnapshot,
    referenced: &[String],
) -> Result<()> {
    let prefix = format!(
        "{}@{}_{}",
        latest.dataset,
        source.snapshot.prefix,
        sanitize_snapshot_component(&source.id)
    );
    let output = command_stdout(Command::new("zfs").args([
        "list",
        "-H",
        "-t",
        "snapshot",
        "-o",
        "name",
        "-s",
        "creation",
        "-r",
        &latest.dataset,
    ]))?;
    let snapshots: Vec<_> = output
        .lines()
        .filter(|name| snapshot_cycle_suffix_matches(name, &prefix))
        .map(str::to_string)
        .collect();
    // Always keep the most recent `keep_extra_cycles + 1` snapshots plus the
    // latest, and never delete a snapshot still referenced as a diff base by a
    // lagging/offline destination.
    let keep = source.snapshot.keep_extra_cycles.saturating_add(1);
    let retain_recent = snapshots.len().saturating_sub(keep);
    let referenced: BTreeSet<&str> = referenced.iter().map(String::as_str).collect();
    let mut failures = 0_usize;
    for (index, snapshot) in snapshots.iter().enumerate() {
        if index >= retain_recent {
            break;
        }
        if snapshot == &latest.full_name || referenced.contains(snapshot.as_str()) {
            continue;
        }
        let status = Command::new("zfs")
            .args(["destroy", snapshot])
            .status()
            .with_context(|| format!("failed to execute zfs destroy {snapshot}"))?;
        if !status.success() {
            // One held/busy snapshot must not block reclaiming the rest —
            // bailing here let a single user hold pin every later snapshot.
            warn!(
                snapshot,
                "zfs destroy failed; continuing with remaining snapshots"
            );
            failures += 1;
        }
    }
    if failures > 0 {
        warn!(
            failures,
            "some superseded source snapshots could not be destroyed"
        );
    }
    Ok(())
}

/// `<dataset>@<prefix>` names match only when the remainder after `prefix` is
/// exactly `_` + a 12-digit cycle number. A bare starts_with let source id
/// "docs" reclaim "docs_old"'s snapshots (including diff bases another
/// destination still referenced), and destination id "a" destroy the
/// (src, "a_b") pair's current baseline.
fn snapshot_cycle_suffix_matches(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|digits| digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Relative paths (under `live_root`) changed between a snapshot and the
/// LIVE filesystem (`zfs diff -H <snapshot>` with no second argument diffs
/// against the current state). Same contract as [`zfs_diff_changed_paths`]:
/// `None` means "no reliable diff — fall back".
fn zfs_diff_changed_paths_live(base_full_name: &str, live_root: &Path) -> Option<Vec<DiffPath>> {
    let base_exists = Command::new("zfs")
        .args(["list", "-H", "-t", "snapshot", base_full_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !base_exists {
        return None;
    }
    let output = Command::new("zfs")
        .args(["diff", "-H", base_full_name])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(parse_zfs_diff(&text, live_root))
}

/// The ZFS dataset an arbitrary local path lives on (deepest mountpoint that
/// contains it). Errors when the path is not on a mounted ZFS dataset.
fn resolve_dataset_for_path(path: &Path) -> Result<ZfsDataset> {
    let canonical = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    let mut best: Option<(String, PathBuf)> = None;
    for (name, mountpoint) in zfs_filesystems()? {
        if canonical.starts_with(&mountpoint) {
            let replace = best
                .as_ref()
                .map(|(_, current)| mountpoint.components().count() > current.components().count())
                .unwrap_or(true);
            if replace {
                best = Some((name, mountpoint));
            }
        }
    }
    let Some((name, mountpoint)) = best else {
        bail!("path is not on a zfs dataset: {}", canonical.display());
    };
    ensure_no_nested_mounts(&name, &canonical)?;
    let path_in_dataset = path_in_dataset(&canonical, &mountpoint)?;
    Ok(ZfsDataset {
        name,
        mountpoint,
        path_in_dataset,
    })
}

/// Refuse ZFS snapshot/diff use when foreign filesystems are mounted inside
/// the tree the dataset is expected to cover (W6): their contents belong to
/// another filesystem, so this dataset's snapshots would read the subtree as
/// missing (a snapshot-backed mirror would DELETE it on the destination) and
/// `zfs diff` would never report its changes. Callers degrade to the live
/// manifest walk, which sees through mount points.
fn ensure_no_nested_mounts(dataset: &str, live_root: &Path) -> Result<()> {
    let nested = nested_mounts_under(live_root);
    if let Some(first) = nested.first() {
        bail!(
            "{} nested filesystem(s) mounted under {} (e.g. {}): their contents are invisible \
             to snapshots and zfs diff of dataset {dataset}; unmount them or use the manifest \
             backend for this tree",
            nested.len(),
            live_root.display(),
            first.display()
        );
    }
    Ok(())
}

/// Mount points strictly below `live_root`, read from `/proc/self/mounts`.
/// ZFS `.zfs` control-dir automounts (visited snapshots) are not real
/// nesting and are excluded.
#[cfg(target_os = "linux")]
fn nested_mounts_under(live_root: &Path) -> Vec<PathBuf> {
    let Ok(canonical) = live_root.canonicalize() else {
        return Vec::new();
    };
    let Ok(mounts) = fs::read_to_string("/proc/self/mounts") else {
        return Vec::new();
    };
    nested_mounts_in(&mounts, &canonical)
}

#[cfg(not(target_os = "linux"))]
fn nested_mounts_under(_live_root: &Path) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(any(target_os = "linux", test))]
fn nested_mounts_in(mounts: &str, canonical_root: &Path) -> Vec<PathBuf> {
    let mut nested = Vec::new();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let Some(_device) = fields.next() else {
            continue;
        };
        let Some(raw_mount) = fields.next() else {
            continue;
        };
        let mount_point = PathBuf::from(crate::core::storage::unescape_mount_field(raw_mount));
        if mount_point == canonical_root || !mount_point.starts_with(canonical_root) {
            continue;
        }
        let Ok(rel) = mount_point.strip_prefix(canonical_root) else {
            continue;
        };
        if rel.components().any(|c| c.as_os_str() == ".zfs") {
            continue;
        }
        nested.push(mount_point);
    }
    nested.sort();
    nested.dedup();
    nested
}

/// Snapshot-name prefix for one (source, destination) pair's dst-side
/// verified baselines, used both to name new ones and to sweep stale ones.
fn dst_baseline_prefix(source: &SourceGroupConfig, destination_id: &str) -> String {
    format!(
        "{}_dstbase_{}_{}",
        source.snapshot.prefix,
        sanitize_snapshot_component(&source.id),
        sanitize_snapshot_component(destination_id)
    )
}

/// What a verifying pass may do to the DESTINATION-side baseline snapshot.
/// The invariant every zfs-diff shortcut rests on: a stored dst baseline's
/// content equals its paired source baseline's content ACROSS THE WHOLE
/// TREE. Only passes that actually established whole-tree equality may
/// refresh it — a source-side-only pass refreshing the dst baseline would
/// bake any dst drift that happened since the previous baseline into the
/// new one, permanently hiding it from future zfs-diff Compares/Fulls.
enum DstBaselineAction<'a> {
    /// This pass made dst equal the source base across the whole tree
    /// (walk-based full reconcile, full manifest sync, dual-side zfs Full):
    /// snapshot the dst dataset now as the new baseline. `touched` is the set
    /// of relative paths this pass itself wrote/removed; when provided (and a
    /// previous baseline exists) the refresh is cross-checked with
    /// `zfs diff old-base new-base` — any non-directory change OUTSIDE the
    /// set means an external writer hit the destination between the
    /// comparison and the snapshot, and its drift would be baked into the new
    /// baseline (invisible to every future diff-based Compare/Full). The
    /// refresh then downgrades to Retain.
    Refresh {
        dst_root: &'a Path,
        touched: Option<&'a BTreeSet<String>>,
    },
    /// This pass only applied source-side changes (zfs-diff incremental):
    /// keep the previous dst baseline — it still describes the last
    /// whole-tree verified state. Compare's dst diff just grows until the
    /// next whole-tree pass, staying correct.
    Retain,
    /// Not a local ZFS destination (or the feature is off): drop any stored
    /// baseline so nothing stale lingers.
    Clear,
}

/// Record the verified baselines for a destination: the source-side ZFS
/// snapshot (diff base for incremental syncs) always advances with the
/// verify; the destination-side baseline follows `action` (see
/// [`DstBaselineAction`]). Baseline failures never fail the sync: the dst
/// base is cleared and Compare falls back to the full walk.
fn record_destination_verified_baselines(
    state: &State,
    source: &SourceGroupConfig,
    destination_id: &str,
    src_snapshot_name: Option<&str>,
    action: DstBaselineAction<'_>,
    cycle_id: i64,
) -> Result<()> {
    state.set_destination_verified_snapshot(&source.id, destination_id, src_snapshot_name)?;
    let (dst_root, touched) = match action {
        DstBaselineAction::Retain => return Ok(()),
        DstBaselineAction::Clear => (None, None),
        // A dst baseline without a paired source base is meaningless.
        DstBaselineAction::Refresh { .. } if src_snapshot_name.is_none() => (None, None),
        DstBaselineAction::Refresh { dst_root, touched } => (Some(dst_root), touched),
    };
    let previous = state
        .destination_verified_dst_snapshot(&source.id, destination_id)
        .unwrap_or(None);
    let dst_base = match dst_root {
        Some(dst_root) => {
            match create_dst_baseline_snapshot(source, destination_id, dst_root, cycle_id) {
                Ok(name) => {
                    match baseline_refresh_contaminated(
                        previous.as_deref(),
                        &name,
                        dst_root,
                        touched,
                    ) {
                        Some(outside) => {
                            // External write during the pass: baking it into
                            // the baseline would hide it from every future
                            // diff-based Compare/Full. Keep the old baseline
                            // (Retain semantics — the drift stays visible in
                            // the dst-side diff until repaired).
                            warn!(
                                source = source.id,
                                destination = destination_id,
                                path = outside,
                                "destination changed outside this pass during the refresh window; \
                                 keeping the previous baseline"
                            );
                            Command::new("zfs").args(["destroy", &name]).status().ok();
                            return Ok(());
                        }
                        None => Some(name),
                    }
                }
                Err(err) => {
                    debug!(
                        source = source.id,
                        destination = destination_id,
                        error = %err,
                        "no destination-side baseline snapshot (compare falls back to full walk)"
                    );
                    None
                }
            }
        }
        None => None,
    };
    state.set_destination_verified_dst_snapshot(&source.id, destination_id, dst_base.as_deref())?;
    cleanup_dst_baseline_snapshots(
        source,
        destination_id,
        dst_base.as_deref(),
        previous.as_deref(),
    );
    Ok(())
}

/// Cross-check a baseline refresh: any non-directory path in
/// `zfs diff -HF <previous> <new>` that this pass did not itself touch is
/// evidence of an external writer during the pass window. Returns the first
/// such path. `None` when clean or when the check cannot run (no previous
/// baseline / different dataset / no touched set supplied) — refreshes then
/// proceed as before.
fn baseline_refresh_contaminated(
    previous: Option<&str>,
    new_base: &str,
    dst_root: &Path,
    touched: Option<&BTreeSet<String>>,
) -> Option<String> {
    let touched = touched?;
    let previous = previous?;
    if previous.split('@').next() != new_base.split('@').next() {
        return None;
    }
    let live_root = dst_root.canonicalize().ok()?;
    let output = command_stdout(Command::new("zfs").args(["diff", "-HF", previous, new_base]))
        .map_err(|err| {
            debug!(error = %err, "baseline refresh cross-check diff failed; accepting refresh");
            err
        })
        .ok()?;
    for line in output.lines() {
        let mut fields = line.split('\t');
        let Some(change) = fields.next() else {
            continue;
        };
        if !matches!(change, "-" | "+" | "M" | "R") {
            continue;
        }
        let Some(file_type) = fields.next() else {
            continue;
        };
        // Directory rows are noise: the pass legitimately touches every
        // directory (mtime/mode stamping) without that being drift.
        if file_type == "/" {
            continue;
        }
        for raw in fields {
            let abs = unescape_zfs_path(raw);
            let Ok(rel) = Path::new(&abs).strip_prefix(&live_root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            let Ok(rel_str) = rel_to_string(rel) else {
                continue;
            };
            if rel_str_is_internal(&rel_str) || touched.contains(&rel_str) {
                continue;
            }
            return Some(rel_str);
        }
    }
    None
}

fn create_dst_baseline_snapshot(
    source: &SourceGroupConfig,
    destination_id: &str,
    dst_root: &Path,
    cycle_id: i64,
) -> Result<String> {
    let dataset = resolve_dataset_for_path(dst_root)?;
    let full_name = format!(
        "{}@{}_{:012}",
        dataset.name,
        dst_baseline_prefix(source, destination_id),
        cycle_id
    );
    // A baseline Refresh must capture THIS pass's verified state: any
    // same-name snapshot older than moments ago is a stale leftover (e.g.
    // from a previous database generation) and gets recreated.
    ensure_zfs_snapshot(&full_name, Some(Utc::now().timestamp()))?;
    Ok(full_name)
}

/// Best-effort sweep of this pair's superseded dst baselines: everything
/// with the pair's prefix except the current one (only the latest base is
/// ever diffed against).
fn cleanup_dst_baseline_snapshots(
    source: &SourceGroupConfig,
    destination_id: &str,
    current: Option<&str>,
    previous: Option<&str>,
) {
    let Some(dataset) = current
        .or(previous)
        .and_then(|name| name.split('@').next())
        .map(str::to_string)
    else {
        return;
    };
    let prefix = format!("{dataset}@{}", dst_baseline_prefix(source, destination_id));
    let Ok(output) = command_stdout(
        Command::new("zfs").args(["list", "-H", "-t", "snapshot", "-o", "name", "-r", &dataset]),
    ) else {
        return;
    };
    for snapshot in output
        .lines()
        .filter(|name| snapshot_cycle_suffix_matches(name, &prefix))
    {
        if Some(snapshot) == current {
            continue;
        }
        match Command::new("zfs").args(["destroy", snapshot]).status() {
            Ok(status) if status.success() => {}
            // Held/busy snapshots were silently treated as destroyed before;
            // the next verified pass retries, but the leak must be visible.
            Ok(_) => warn!(snapshot, "zfs destroy of superseded dst baseline failed"),
            Err(err) => {
                warn!(snapshot, error = %err, "failed to run zfs destroy for dst baseline");
            }
        }
    }
}

fn command_stdout(command: &mut Command) -> Result<String> {
    let output = command.output().context("failed to execute command")?;
    if !output.status.success() {
        bail!(
            "command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn sanitize_snapshot_component(value: &str) -> String {
    value
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

#[derive(Debug, Clone)]
enum SourceEndpoint {
    Dir { root: PathBuf, add_directory: bool },
    File { path: PathBuf },
}

impl SourceEndpoint {
    fn resolve(source: &SourceGroupConfig) -> Result<Self> {
        let metadata = fs::symlink_metadata(&source.src)
            .with_context(|| format!("failed to read source {}", source.src.display()))?;
        if metadata.is_dir() {
            return Ok(Self::Dir {
                root: source.src.clone(),
                add_directory: source.add_directory,
            });
        }
        if metadata.file_type().is_symlink() && fs::metadata(&source.src).is_ok_and(|m| m.is_dir())
        {
            // Deliberate lstat semantics, but surprising enough to say out
            // loud: the LINK syncs as a single file, not the tree behind it.
            warn!(
                source = source.id,
                path = %source.src.display(),
                "source root is a symlink to a directory; syncing the link itself, \
                 not the directory tree (point src at the real path to sync the tree)"
            );
        }
        if metadata.is_file() || metadata.file_type().is_symlink() {
            return Ok(Self::File {
                path: source.src.clone(),
            });
        }
        bail!("source path is neither a file nor a directory");
    }

    fn snapshot(&self, excludes: &[PathBuf], checksum: bool) -> Result<Vec<SnapshotEntry>> {
        match self {
            Self::Dir { root, .. } => {
                take_snapshot_with_excludes(root, SnapshotMode::Source, excludes, checksum)
            }
            Self::File { path } => {
                let rel_path = file_name_string(path)?;
                if is_rel_excluded(Path::new(&rel_path), excludes) {
                    return Ok(Vec::new());
                }
                Ok(vec![snapshot_entry(path, rel_path, checksum)?])
            }
        }
    }
}

#[derive(Debug, Clone)]
enum DestinationEndpoint {
    Dir { root: PathBuf },
    File { path: PathBuf },
}

impl DestinationEndpoint {
    fn resolve(source: &SourceEndpoint, dst: &DestinationConfig) -> Result<Self> {
        reject_dangerous_destination(&dst.path)?;
        match source {
            SourceEndpoint::Dir {
                root: src_root,
                add_directory,
            } => {
                if dst.path.exists() && !dst.path.is_dir() {
                    bail!("directory source cannot sync to non-directory destination");
                }
                let effective_root = if *add_directory {
                    let dir_name = src_root.file_name().ok_or_else(|| {
                        anyhow::anyhow!("source directory has no name: {}", src_root.display())
                    })?;
                    dst.path.join(dir_name)
                } else {
                    dst.path.clone()
                };
                // The scanners use lstat and read a symlink root as an EMPTY
                // tree: every pass would re-copy everything through the link
                // while mirror deletion never ran — permanently green and
                // permanently wrong. Refuse it loudly instead.
                if fs::symlink_metadata(&effective_root)
                    .map(|meta| meta.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    bail!(
                        "destination root {} is a symlink; point the destination at the real path",
                        effective_root.display()
                    );
                }
                Ok(Self::Dir {
                    root: effective_root,
                })
            }
            SourceEndpoint::File { .. } => {
                if !dst.path.exists() || dst.path.is_dir() {
                    Ok(Self::Dir {
                        root: dst.path.clone(),
                    })
                } else {
                    Ok(Self::File {
                        path: dst.path.clone(),
                    })
                }
            }
        }
    }

    fn check_online(&self) -> Result<()> {
        match self {
            Self::Dir { root } => check_destination_online(root),
            Self::File { path } => check_file_destination_online(path),
        }
    }
}

fn reject_dangerous_destination(path: &Path) -> Result<()> {
    let normalized = normalize_existing_or_raw(path);
    let critical = [
        Path::new("/"),
        Path::new("/dev"),
        Path::new("/proc"),
        Path::new("/sys"),
        Path::new("/run"),
        Path::new("/boot"),
        Path::new("/etc"),
        Path::new("/bin"),
        Path::new("/sbin"),
        Path::new("/usr"),
        Path::new("/lib"),
        Path::new("/lib64"),
    ];
    if critical.iter().any(|critical| {
        if *critical == Path::new("/") {
            normalized == *critical
        } else {
            normalized == *critical || normalized.starts_with(critical)
        }
    }) {
        bail!(
            "refusing to use system path as destination: {}",
            normalized.display()
        );
    }
    Ok(())
}

fn normalize_existing_or_raw(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        let mut out = PathBuf::new();
        for part in path.components() {
            out.push(part.as_os_str());
        }
        out
    })
}

fn sync_endpoint(
    source: &SourceEndpoint,
    dst: &DestinationEndpoint,
    destination_id: &str,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    match (source, dst) {
        (
            SourceEndpoint::Dir { root: src_root, .. },
            DestinationEndpoint::Dir { root: dst_root },
        ) => sync_destination(
            src_root,
            dst_root,
            destination_id,
            cycle_id,
            source_snapshot,
            excludes,
            sync,
        ),
        (SourceEndpoint::Dir { .. }, DestinationEndpoint::File { .. }) => {
            bail!("directory source cannot sync to a destination file")
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::Dir { root }) => {
            let rel_path = file_name_string(path)?;
            if is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            sync_file_to_path(
                path,
                root,
                &root.join(rel_path),
                destination_id,
                cycle_id,
                sync,
            )
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::File { path: dst_path }) => {
            let rel_path = file_name_string(path)?;
            if is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            let parent = dst_path
                .parent()
                .ok_or_else(|| anyhow!("destination file path has no parent"))?;
            sync_file_to_path(path, parent, dst_path, destination_id, cycle_id, sync)
        }
    }
}

fn sync_destination(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    source_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    fs::create_dir_all(dst_root).with_context(|| {
        format!(
            "failed to create destination directory: {}",
            dst_root.display()
        )
    })?;
    let result = (|| {
        let dst_snapshot =
            take_snapshot_with_excludes(dst_root, SnapshotMode::Destination, &[], sync.checksum)?;
        // Same compare implementation the Scan report uses.
        let diff = diff_manifests(source_snapshot, &dst_snapshot, excludes, sync, false);

        for entry in source_snapshot.iter().filter(|e| e.file_type == "dir") {
            let target = dst_root.join(&entry.rel_path);
            if path_blocks_directory(&target) {
                move_to_trash(dst_root, &entry.rel_path, cycle_id)?;
            }
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create directory {}", target.display()))?;
            // Directory mode is applied at end-of-cycle (deepest-first) so a
            // read-only source dir does not block writing its children.
        }

        let to_copy: Vec<&SnapshotEntry> = diff
            .entries_to_copy()
            .into_iter()
            .map(|(entry, _)| entry)
            .collect();
        let total_bytes: u64 = to_copy
            .iter()
            .filter(|e| e.file_type == "file")
            .map(|e| e.size.max(0) as u64)
            .sum();
        let transfer_guard = progress::begin_transfer(destination_id, dst_root, total_bytes);
        let outcome =
            copy_entries_parallel(src_root, dst_root, destination_id, cycle_id, &to_copy, sync)?;
        drop(transfer_guard);

        if sync.mirror {
            for rel in diff.extra_paths_deepest_first() {
                move_to_trash(dst_root, &rel, cycle_id)
                    .with_context(|| format!("failed to remove extra destination path {rel}"))?;
            }
        }

        // Content-equal files whose permission bits drifted: chmod in place.
        for entry in &diff.mode_fixes {
            let path = safe_join_rel(dst_root, &entry.rel_path)?;
            set_mode(&path, entry.mode)
                .with_context(|| format!("failed to set mode on {}", entry.rel_path))?;
        }

        set_snapshot_dir_mtimes(dst_root, source_snapshot)?;
        // Verify what this cycle wrote; untouched entries were compared against
        // the fresh destination scan above, so re-walking the tree is redundant.
        verify_copied_entries(
            dst_root,
            to_copy.iter().copied(),
            &outcome.unverifiable_paths(),
            sync,
        )?;
        outcome.into_result()
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    cleanup_expired_trash(dst_root, sync.trash_keep_days);
    result
}

/// Returns the source manifest plus the set of relative paths this pass
/// itself wrote, removed or chmod'd — the baseline refresh cross-checks it
/// against `zfs diff` to detect external writers (see
/// [`baseline_refresh_contaminated`]).
fn sync_destination_fast_missing_dirs(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<(Vec<SnapshotEntry>, BTreeSet<String>)> {
    fs::create_dir_all(dst_root).with_context(|| {
        format!(
            "failed to create destination directory: {}",
            dst_root.display()
        )
    })?;
    let result = (|| {
        let mut changing_paths = BTreeSet::new();
        info!(
            destination = destination_id,
            "reconcile: scanning source and destination trees concurrently"
        );
        // The two trees are independent (typically different disks or pools):
        // scanning them concurrently roughly halves the reconcile's dominant
        // scan phase versus the old serial dst-then-src walk.
        let cancel_token = cancel::current_token();
        let (dst_result, source_result) = thread::scope(|scope| {
            let src_handle = scope.spawn(|| {
                let _cancel = cancel::enter(cancel_token.clone());
                take_snapshot_with_excludes(src_root, SnapshotMode::Source, excludes, sync.checksum)
            });
            let dst_result = take_snapshot_with_excludes(
                dst_root,
                SnapshotMode::Destination,
                &[],
                sync.checksum,
            );
            (
                dst_result,
                src_handle.join().expect("source scan thread panicked"),
            )
        });
        let dst_snapshot = dst_result?;
        let source_snapshot = source_result?;
        let dst_map = map_entry_refs(&dst_snapshot);
        let source_map = map_entry_refs(&source_snapshot);

        // Everything this pass writes/removes/chmods, for the baseline
        // refresh cross-check.
        let mut touched: BTreeSet<String> = BTreeSet::new();

        // Missing/blocked directories (snapshot order is breadth-first, so
        // parents come before children). Directory mode is applied at
        // end-of-cycle (deepest-first) so a read-only source dir does not
        // block writing its children.
        for entry in source_snapshot.iter().filter(|e| e.file_type == "dir") {
            if dst_map
                .get(entry.rel_path.as_str())
                .is_some_and(|existing| existing.file_type == "dir")
            {
                continue;
            }
            let target = dst_root.join(&entry.rel_path);
            if path_blocks_directory(&target) {
                move_to_trash(dst_root, &entry.rel_path, cycle_id)?;
            }
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create directory {}", target.display()))?;
            touched.insert(entry.rel_path.clone());
        }

        let to_copy: Vec<&SnapshotEntry> = source_snapshot
            .iter()
            .filter(|e| e.file_type == "file" || e.file_type == "symlink")
            .filter(|e| match dst_map.get(e.rel_path.as_str()) {
                Some(existing) => !entries_match(e, existing, sync),
                None => true,
            })
            .collect();
        info!(
            destination = destination_id,
            source_entries = source_snapshot.len(),
            dst_entries = dst_snapshot.len(),
            to_copy = to_copy.len(),
            "reconcile: copying changed/missing files"
        );
        let total_bytes: u64 = to_copy
            .iter()
            .filter(|e| e.file_type == "file")
            .map(|e| e.size.max(0) as u64)
            .sum();
        let transfer_guard = progress::begin_transfer(destination_id, dst_root, total_bytes);
        let outcome =
            copy_entries_parallel(src_root, dst_root, destination_id, cycle_id, &to_copy, sync)?;
        drop(transfer_guard);
        touched.extend(to_copy.iter().map(|e| e.rel_path.clone()));

        // Content-equal files whose permission bits drifted: chmod in place.
        for entry in source_snapshot.iter().filter(|e| e.file_type == "file") {
            if let Some(existing) = dst_map.get(entry.rel_path.as_str()) {
                if entries_match(entry, existing, sync) && entry_mode_differs(entry, existing) {
                    let path = safe_join_rel(dst_root, &entry.rel_path)?;
                    set_mode(&path, entry.mode)
                        .with_context(|| format!("failed to set mode on {}", entry.rel_path))?;
                    touched.insert(entry.rel_path.clone());
                }
            }
        }

        if sync.mirror {
            let mut extra_paths: Vec<String> = dst_map
                .keys()
                .filter(|rel| {
                    !source_map.contains_key(*rel) && !is_rel_excluded(Path::new(rel), excludes)
                })
                .map(|rel| rel.to_string())
                .collect();
            extra_paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
            for rel in extra_paths {
                move_to_trash(dst_root, &rel, cycle_id)
                    .with_context(|| format!("failed to remove extra destination path {rel}"))?;
                touched.insert(rel);
            }
        }

        set_snapshot_dir_mtimes(dst_root, &source_snapshot)?;
        info!(
            destination = destination_id,
            "reconcile: verifying copied entries"
        );
        // Verify everything this cycle wrote (the changed/missing batch,
        // whole missing subtrees included). Untouched entries were compared
        // against the fresh destination scan above.
        let mut ignored = outcome.unverifiable_paths();
        ignored.extend(changing_paths.iter().cloned());
        verify_copied_entries(dst_root, to_copy.iter().copied(), &ignored, sync)?;
        info!(destination = destination_id, "reconcile: verified ok");
        changing_paths.extend(outcome.changing.iter().cloned());
        let failed_count = outcome.failed.len();
        if let Some((path, err)) = outcome.failed.into_iter().next() {
            return Err(err.context(format!(
                "{failed_count} file transfer(s) failed (first: {path})"
            )));
        }
        if !changing_paths.is_empty() {
            return Err(source_changing_error(&changing_paths));
        }
        Ok((source_snapshot, touched))
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    cleanup_expired_trash(dst_root, sync.trash_keep_days);
    result
}

#[allow(clippy::too_many_arguments)]
fn sync_endpoint_event_paths(
    source: &SourceEndpoint,
    dst: &DestinationEndpoint,
    destination_id: &str,
    cycle_id: i64,
    rel_paths: &[DiffPath],
    force_hash_paths: &[DiffPath],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    match (source, dst) {
        (
            SourceEndpoint::Dir { root: src_root, .. },
            DestinationEndpoint::Dir { root: dst_root },
        ) => sync_destination_event_paths(
            src_root,
            dst_root,
            destination_id,
            cycle_id,
            rel_paths,
            force_hash_paths,
            excludes,
            sync,
        ),
        (SourceEndpoint::Dir { .. }, DestinationEndpoint::File { .. }) => {
            bail!("directory source cannot sync to a destination file")
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::Dir { root }) => {
            let rel_path = file_name_string(path)?;
            if rel_paths.is_empty() || is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            sync_file_to_path(
                path,
                root,
                &root.join(rel_path),
                destination_id,
                cycle_id,
                sync,
            )
        }
        (SourceEndpoint::File { path }, DestinationEndpoint::File { path: dst_path }) => {
            let rel_path = file_name_string(path)?;
            if rel_paths.is_empty() || is_rel_excluded(Path::new(&rel_path), excludes) {
                return Ok(());
            }
            let parent = dst_path
                .parent()
                .ok_or_else(|| anyhow!("destination file path has no parent"))?;
            sync_file_to_path(path, parent, dst_path, destination_id, cycle_id, sync)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn sync_destination_event_paths(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    rel_paths: &[DiffPath],
    force_hash_paths: &[DiffPath],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
) -> Result<()> {
    if rel_paths.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(dst_root).with_context(|| {
        format!(
            "failed to create destination directory: {}",
            dst_root.display()
        )
    })?;
    let result = (|| {
        let mut source_snapshot = take_snapshot_diff_paths_with_excludes(
            src_root,
            rel_paths,
            SnapshotMode::Source,
            excludes,
            sync.checksum,
        )?;
        let mut dst_snapshot = take_snapshot_diff_paths_with_excludes(
            dst_root,
            rel_paths,
            SnapshotMode::Destination,
            &[],
            sync.checksum,
        )?;
        if !sync.checksum {
            add_hash_evidence(
                src_root,
                force_hash_paths,
                SnapshotMode::Source,
                excludes,
                &mut source_snapshot,
            )?;
            add_hash_evidence(
                dst_root,
                force_hash_paths,
                SnapshotMode::Destination,
                &[],
                &mut dst_snapshot,
            )?;
        }
        let mut changing_paths = BTreeSet::new();
        let total_bytes: u64 = source_snapshot
            .iter()
            .filter(|e| e.file_type == "file")
            .map(|e| e.size.max(0) as u64)
            .sum();
        let transfer_guard = progress::begin_transfer(destination_id, dst_root, total_bytes);
        sync_changed_entries_local(
            src_root,
            dst_root,
            destination_id,
            cycle_id,
            rel_paths,
            &source_snapshot,
            &dst_snapshot,
            excludes,
            sync,
            &mut changing_paths,
        )?;
        drop(transfer_guard);

        let mut actual = take_snapshot_diff_paths_with_excludes(
            dst_root,
            rel_paths,
            SnapshotMode::Destination,
            &[],
            sync.checksum,
        )?;
        if !sync.checksum {
            add_hash_evidence(
                dst_root,
                force_hash_paths,
                SnapshotMode::Destination,
                &[],
                &mut actual,
            )?;
        }
        verify_snapshot_entries(&source_snapshot, &actual, &changing_paths, excludes, sync)?;
        if !changing_paths.is_empty() {
            return Err(source_changing_error(&changing_paths));
        }
        Ok(())
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    cleanup_expired_trash(dst_root, sync.trash_keep_days);
    result
}

#[allow(clippy::too_many_arguments)]
fn sync_changed_entries_local(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    rel_paths: &[DiffPath],
    source_snapshot: &[SnapshotEntry],
    dst_snapshot: &[SnapshotEntry],
    excludes: &[PathBuf],
    sync: &NativeSyncConfig,
    changing_paths: &mut BTreeSet<String>,
) -> Result<()> {
    let source_map = map_entry_refs(source_snapshot);
    let dst_map = map_entry_refs(dst_snapshot);

    let mut type_mismatch: Vec<String> = source_snapshot
        .iter()
        .filter(|entry| {
            dst_map
                .get(entry.rel_path.as_str())
                .is_some_and(|existing| existing.file_type != entry.file_type)
        })
        .map(|entry| entry.rel_path.clone())
        .collect();
    type_mismatch.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
    for rel in type_mismatch {
        move_to_trash(dst_root, &rel, cycle_id)
            .with_context(|| format!("failed to replace destination path {rel}"))?;
    }

    for entry in source_snapshot.iter().filter(|e| e.file_type == "dir") {
        let target = dst_root.join(&entry.rel_path);
        fs::create_dir_all(&target)
            .with_context(|| format!("failed to create directory {}", target.display()))?;
        // Mode applied at end via set_snapshot_dir_mtimes (deepest-first).
    }

    // Event/diff paths carry positive change evidence: exact comparison, so a
    // same-size rewrite within the modify window still copies.
    let to_copy: Vec<&SnapshotEntry> = source_snapshot
        .iter()
        .filter(|e| e.file_type == "file" || e.file_type == "symlink")
        .filter(|e| match dst_map.get(e.rel_path.as_str()) {
            Some(existing) => !entries_match_exact(e, existing, sync),
            None => true,
        })
        .collect();
    let outcome =
        copy_entries_parallel(src_root, dst_root, destination_id, cycle_id, &to_copy, sync)?;
    changing_paths.extend(outcome.changing.iter().cloned());
    let failed_count = outcome.failed.len();
    if let Some((path, err)) = outcome.failed.into_iter().next() {
        // The rest of the batch still copied; surface the per-file failures so
        // the destination goes red and the next cycle retries just these.
        return Err(err.context(format!(
            "{failed_count} file transfer(s) failed (first: {path})"
        )));
    }

    // Content-equal files whose permission bits drifted: chmod in place.
    for entry in source_snapshot.iter() {
        if let Some(existing) = dst_map.get(entry.rel_path.as_str()) {
            if entries_match_exact(entry, existing, sync) && entry_mode_differs(entry, existing) {
                let path = safe_join_rel(dst_root, &entry.rel_path)?;
                set_mode(&path, entry.mode)
                    .with_context(|| format!("failed to set mode on {}", entry.rel_path))?;
            }
        }
    }

    if sync.mirror {
        // rel_str_is_internal keeps the trash itself out of the delete set —
        // trashing the trash renames it into its own subtree (fails) and the
        // failure fallback would erase the recycle bin.
        let mut extra_paths: Vec<String> = dst_map
            .keys()
            .filter(|rel| {
                !source_map.contains_key(*rel)
                    && !rel_str_is_internal(rel)
                    && !is_rel_excluded(Path::new(rel), excludes)
            })
            .map(|rel| rel.to_string())
            .collect();
        for path in rel_paths {
            let rel = path.rel.as_str();
            if !source_map.contains_key(rel)
                && !rel_str_is_internal(rel)
                && !is_rel_excluded(Path::new(rel), excludes)
            {
                extra_paths.push(rel.to_string());
            }
        }
        extra_paths.sort_by(|a, b| path_depth(b).cmp(&path_depth(a)).then_with(|| b.cmp(a)));
        extra_paths.dedup();
        for rel in extra_paths {
            move_to_trash(dst_root, &rel, cycle_id)
                .with_context(|| format!("failed to remove changed destination path {rel}"))?;
        }
    }
    set_snapshot_dir_mtimes(dst_root, source_snapshot)?;
    Ok(())
}

fn sync_file_to_path(
    src_path: &Path,
    dst_root: &Path,
    final_path: &Path,
    destination_id: &str,
    cycle_id: i64,
    sync: &NativeSyncConfig,
) -> Result<()> {
    let result = (|| {
        if final_path.exists() && final_path.is_dir() {
            bail!(
                "destination file target is a directory: {}",
                final_path.display()
            );
        }
        let rel_path = file_name_string(final_path)?;
        let entry = snapshot_entry(src_path, rel_path, sync.checksum)?;
        let existing = if final_path.exists() || fs::symlink_metadata(final_path).is_ok() {
            Some(snapshot_entry(
                final_path,
                entry.rel_path.clone(),
                sync.checksum,
            )?)
        } else {
            None
        };
        let needs_copy = existing
            .as_ref()
            .map(|existing| !entries_match(&entry, existing, sync))
            .unwrap_or(true);

        if needs_copy {
            let total_bytes = if entry.file_type == "file" {
                entry.size.max(0) as u64
            } else {
                0
            };
            let transfer_guard = progress::begin_transfer(destination_id, dst_root, total_bytes);
            copy_single_entry(
                src_path,
                dst_root,
                destination_id,
                cycle_id,
                &entry,
                final_path,
            )?;
            drop(transfer_guard);
        }
        verify_file_target(final_path, &entry, sync)?;
        Ok(())
    })();
    cleanup_tmp_cycle(dst_root, cycle_id);
    cleanup_expired_trash(dst_root, sync.trash_keep_days);
    result
}

fn copy_single_entry(
    src: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent {}", parent.display()))?;
    }
    match entry.file_type.as_str() {
        "file" => copy_file(src, dst_root, destination_id, cycle_id, entry, final_path),
        "symlink" => copy_symlink(src, dst_root, cycle_id, entry, final_path),
        other => Err(anyhow!("unsupported single source type {other}")),
    }
}

fn verify_file_target(
    final_path: &Path,
    expected: &SnapshotEntry,
    sync: &NativeSyncConfig,
) -> Result<()> {
    if !final_path.exists() && fs::symlink_metadata(final_path).is_err() {
        bail!("destination missing {}", final_path.display());
    }
    let actual = snapshot_entry(final_path, expected.rel_path.clone(), sync.checksum)?;
    if !entries_match(expected, &actual, sync) {
        bail!("destination mismatch at {}", final_path.display());
    }
    Ok(())
}

pub fn take_snapshot(root: &Path, mode: SnapshotMode) -> Result<Vec<SnapshotEntry>> {
    take_snapshot_with_excludes(root, mode, &[], true)
}

fn take_snapshot_with_excludes(
    root: &Path,
    mode: SnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
) -> Result<Vec<SnapshotEntry>> {
    let mut entries = Vec::new();
    take_snapshot_with_excludes_streamed(root, mode, excludes, checksum, &mut |entry| {
        entries.push(entry);
        Ok(())
    })?;
    Ok(entries)
}

/// Walk the tree and hand each entry to `sink` as it is produced, so callers
/// that forward entries elsewhere (the streaming snapshot endpoint) never
/// hold the whole manifest in memory.
fn take_snapshot_with_excludes_streamed(
    root: &Path,
    mode: SnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
    sink: &mut dyn FnMut(SnapshotEntry) -> Result<()>,
) -> Result<()> {
    let scan_progress = progress::start_scan(root);
    let mut entries_seen = 0_u64;
    for_each_breadth_first_snapshot_path(root, root, mode, excludes, |path, metadata| {
        entries_seen += 1;
        let scan_path = if metadata.is_dir() {
            path
        } else {
            path.parent().unwrap_or(root)
        };
        scan_progress.update(scan_path, entries_seen);
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("failed to strip root from {}", path.display()))?;
        let rel_path = rel_to_string(rel)?;
        if let Some(entry) = snapshot_entry_from_metadata(path, rel_path, checksum, metadata)? {
            sink(entry)?;
        }
        Ok(())
    })
}

fn take_snapshot_diff_paths_with_excludes(
    root: &Path,
    paths: &[DiffPath],
    mode: SnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
) -> Result<Vec<SnapshotEntry>> {
    // Sorted order groups every descendant right after its ancestor, so a
    // single "covering" cursor suffices for the ancestor dedup below.
    let mut sorted: Vec<&DiffPath> = paths.iter().collect();
    sorted.sort_by(|a, b| a.rel.cmp(&b.rel).then(b.recursive.cmp(&a.recursive)));
    let mut entries = BTreeMap::new();
    // The last kept path whose subtree walk covers subsequent descendants.
    let mut covering: Option<&str> = None;
    for path in sorted {
        // Internal dirs (trash/tmp/probe) must never be snapshot targets on
        // either side: recursing them turns the recycle bin into "entries".
        if rel_str_is_internal(&path.rel) {
            continue;
        }
        // Ancestor dedup: a recursive path's walk already visits its whole
        // subtree; re-walking each listed descendant (e.g. every `+` line
        // under a created directory) multiplies the IO by the tree depth.
        if let Some(anc) = covering {
            if path.rel.len() > anc.len()
                && path.rel.as_bytes()[anc.len()] == b'/'
                && path.rel.starts_with(anc)
            {
                continue;
            }
        }
        covering = path.recursive.then_some(path.rel.as_str());
        let rel = normalize_rel_path(&path.rel)?;
        if matches!(mode, SnapshotMode::Source) && is_rel_excluded(&rel, excludes) {
            continue;
        }
        let abs = root.join(&rel);
        collect_snapshot_path(
            root,
            &abs,
            mode,
            excludes,
            checksum,
            path.recursive,
            &mut entries,
        )
        .with_context(|| format!("failed to snapshot changed path {}", path.rel))?;
    }
    Ok(entries.into_values().collect())
}

fn collect_snapshot_path(
    root: &Path,
    path: &Path,
    mode: SnapshotMode,
    excludes: &[PathBuf],
    checksum: bool,
    recursive: bool,
    entries: &mut BTreeMap<String, SnapshotEntry>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read metadata {}", path.display()));
        }
    };

    if metadata.is_dir() && recursive {
        // The walk below visits children only: record the directory entry
        // itself too (an empty created directory otherwise never reaches the
        // destination via diff/event paths).
        if path != root {
            let rel = path
                .strip_prefix(root)
                .with_context(|| format!("failed to strip root from {}", path.display()))?;
            match rel_to_string(rel) {
                Ok(rel_path) => {
                    if let Some(entry) =
                        snapshot_entry_from_metadata(path, rel_path, checksum, &metadata)?
                    {
                        entries.insert(entry.rel_path.clone(), entry);
                    }
                }
                Err(_) => {
                    warn!(path = %path.display(), "skipping entry with non-UTF-8 name");
                }
            }
        }
        let scan_progress = progress::start_scan(path);
        let mut entries_seen = 0_u64;
        for_each_breadth_first_snapshot_path(
            root,
            path,
            mode,
            excludes,
            |item_path, item_meta| {
                entries_seen += 1;
                scan_progress.update(item_path, entries_seen);
                let rel = item_path.strip_prefix(root).with_context(|| {
                    format!("failed to strip root from {}", item_path.display())
                })?;
                let Ok(rel_path) = rel_to_string(rel) else {
                    warn!(path = %item_path.display(), "skipping entry with non-UTF-8 name");
                    return Ok(());
                };
                if let Some(entry) =
                    snapshot_entry_from_metadata(item_path, rel_path, checksum, item_meta)?
                {
                    entries.insert(entry.rel_path.clone(), entry);
                }
                Ok(())
            },
        )?;
        return Ok(());
    }

    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("failed to strip root from {}", path.display()))?;
    let Ok(rel_path) = rel_to_string(rel) else {
        warn!(path = %path.display(), "skipping entry with non-UTF-8 name");
        return Ok(());
    };
    if let Some(entry) = snapshot_entry_from_metadata(path, rel_path, checksum, &metadata)? {
        entries.insert(entry.rel_path.clone(), entry);
    }
    Ok(())
}

fn snapshot_entry(path: &Path, rel_path: String, checksum: bool) -> Result<SnapshotEntry> {
    snapshot_entry_if_supported(path, rel_path.clone(), checksum)?
        .ok_or_else(|| anyhow!("unsupported file type at {}", path.display()))
}

fn snapshot_entry_if_supported(
    path: &Path,
    rel_path: String,
    checksum: bool,
) -> Result<Option<SnapshotEntry>> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to read metadata {}", path.display()))?;
    snapshot_entry_from_metadata(path, rel_path, checksum, &metadata)
}

/// [`snapshot_entry_if_supported`] for callers that already hold the entry's
/// metadata (the tree walkers lstat each child to classify it): one lstat per
/// entry instead of re-reading it here.
fn snapshot_entry_from_metadata(
    path: &Path,
    rel_path: String,
    checksum: bool,
    metadata: &fs::Metadata,
) -> Result<Option<SnapshotEntry>> {
    // Per-entry cancellation poll: a single flat directory can hold hundreds
    // of thousands of entries (worse in checksum mode, where each file is
    // fully re-hashed), so per-directory polling alone is not enough.
    cancel::check()?;
    let file_type = if metadata.file_type().is_symlink() {
        "symlink"
    } else if metadata.is_dir() {
        "dir"
    } else if metadata.is_file() {
        "file"
    } else {
        return Ok(None);
    };
    let hash = match file_type {
        "file" if checksum => Some(hash_file(path)?),
        "symlink" => Some(hash_symlink(path)?),
        _ => None,
    };
    Ok(Some(SnapshotEntry {
        rel_path,
        file_type: file_type.to_string(),
        size: metadata.len() as i64,
        mtime_ns: metadata_mtime_ns(metadata)?,
        mode: metadata_mode(metadata),
        hash,
    }))
}

#[derive(Debug, Clone, Copy)]
pub enum SnapshotMode {
    Source,
    Destination,
}

fn for_each_breadth_first_snapshot_path<F>(
    root: &Path,
    start: &Path,
    mode: SnapshotMode,
    excludes: &[PathBuf],
    mut visit: F,
) -> Result<()>
where
    F: FnMut(&Path, &fs::Metadata) -> Result<()>,
{
    let start_metadata = fs::symlink_metadata(start)
        .with_context(|| format!("failed to read metadata {}", start.display()))?;
    if !start_metadata.is_dir() {
        if start != root {
            visit(start, &start_metadata)?;
        }
        return Ok(());
    }
    let mut queue = VecDeque::from([start.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let mut children = sorted_read_dir(&dir)?;
        for child in children.drain(..) {
            if !should_visit_path(root, &child, mode, excludes) {
                continue;
            }
            let metadata = fs::symlink_metadata(&child)
                .with_context(|| format!("failed to read metadata {}", child.display()))?;
            let is_dir = metadata.is_dir();
            visit(&child, &metadata)?;
            if is_dir {
                queue.push_back(child);
            }
        }
    }
    Ok(())
}

fn sorted_read_dir(dir: &Path) -> Result<Vec<PathBuf>> {
    // Every tree walk funnels through here: one cancellation poll per
    // directory keeps even multi-hundred-thousand-entry scans promptly
    // cancellable without instrumenting each walk loop.
    cancel::check()?;
    let mut children = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?
    {
        let entry = entry
            .with_context(|| format!("failed to read directory entry in {}", dir.display()))?;
        children.push(entry.path());
    }
    // OsStr comparison: the previous per-comparison String allocations cost
    // O(n log n) heap traffic on large flat directories.
    children.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    Ok(children)
}

fn should_visit_path(root: &Path, path: &Path, mode: SnapshotMode, excludes: &[PathBuf]) -> bool {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    // auto_sync's own bookkeeping dirs (trash/tmp/probe) are never user data and
    // must be pruned on BOTH sides of the walk. A source that is itself another
    // mapping's destination accumulates `.auto_sync_trash`; without this the
    // source walk treats the recycle bin as content to replicate, and — because
    // the destination walk hides it — every one of those entries shows up as a
    // `to_add` difference that no sync or repair can ever clear.
    if name == INTERNAL_TMP || name == INTERNAL_TRASH || name == INTERNAL_PROBE {
        return false;
    }
    if matches!(mode, SnapshotMode::Source) {
        return !entry_is_excluded(root, path, excludes);
    }
    true
}

/// True when the relative path is (or lives under) one of auto_sync's own
/// destination-side directories (trash/tmp/probe). Mirror deletes rename into
/// the trash, so destination-side `zfs diff` output always names the trash
/// after any delete; letting those paths into a diff union recurses the whole
/// recycle bin into per-path snapshots and reports it as differences.
fn rel_str_is_internal(rel: &str) -> bool {
    rel.split(['/', '\\'])
        .any(|part| part == INTERNAL_TMP || part == INTERNAL_TRASH || part == INTERNAL_PROBE)
}

fn entry_is_excluded(root: &Path, path: &Path, excludes: &[PathBuf]) -> bool {
    if path == root {
        return false;
    }
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    is_rel_excluded(rel, excludes)
}

fn is_rel_excluded(rel: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|exclude| {
        if exclude.as_os_str().to_string_lossy().contains('*') {
            glob_segment_prefix_match(rel, exclude)
        } else {
            rel == exclude || rel.starts_with(exclude)
        }
    })
}

/// Prefix match with `*` as a single-path-segment wildcard, so a pattern like
/// `*/cache` excludes `<any>/cache` and everything under it (used for
/// per-account WeChat cache/temp dirs whose first segment varies). The pattern
/// matches when every one of its segments matches the corresponding leading
/// segment of `rel` (`*` matches any one segment); extra trailing `rel`
/// segments are the excluded subtree.
fn glob_segment_prefix_match(rel: &Path, pattern: &Path) -> bool {
    let star = std::ffi::OsStr::new("*");
    let rel_segs: Vec<_> = rel.iter().collect();
    let pat_segs: Vec<_> = pattern.iter().collect();
    if rel_segs.len() < pat_segs.len() {
        return false;
    }
    pat_segs
        .iter()
        .zip(&rel_segs)
        .all(|(p, r)| *p == star || p == r)
}

fn copy_entry(
    src_root: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
) -> Result<()> {
    let src = src_root.join(&entry.rel_path);
    let final_path = dst_root.join(&entry.rel_path);
    if let Some(parent) = final_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent {}", parent.display()))?;
    }

    match entry.file_type.as_str() {
        "file" => copy_file(&src, dst_root, destination_id, cycle_id, entry, &final_path),
        "symlink" => copy_symlink(&src, dst_root, cycle_id, entry, &final_path),
        other => Err(anyhow!("unsupported entry type {other}")),
    }
}

fn set_snapshot_dir_mtimes(dst_root: &Path, source_snapshot: &[SnapshotEntry]) -> Result<()> {
    let dirs: Vec<TransferDirSpec> = source_snapshot
        .iter()
        .filter(|entry| entry.file_type == "dir")
        .map(|entry| TransferDirSpec {
            rel_path: entry.rel_path.clone(),
            mode: entry.mode,
            mtime_ns: entry.mtime_ns,
        })
        .collect();
    set_dir_mtimes(dst_root, &dirs)
}

fn set_dir_mtimes(root: &Path, dirs: &[TransferDirSpec]) -> Result<()> {
    let mut dirs = dirs.to_vec();
    dirs.sort_by(|a, b| {
        path_depth(&b.rel_path)
            .cmp(&path_depth(&a.rel_path))
            .then_with(|| b.rel_path.cmp(&a.rel_path))
    });
    for dir in &dirs {
        let path = if dir.rel_path.is_empty() {
            root.to_path_buf()
        } else {
            safe_join_rel(root, &dir.rel_path)?
        };
        if !path.exists() {
            continue;
        }
        set_mode(&path, dir.mode).ok();
        let mtime = FileTime::from_unix_time(
            dir.mtime_ns / 1_000_000_000,
            (dir.mtime_ns % 1_000_000_000) as u32,
        );
        set_file_mtime(&path, mtime)
            .with_context(|| format!("failed to set directory mtime {}", path.display()))?;
    }
    Ok(())
}

fn copy_file(
    src: &Path,
    dst_root: &Path,
    destination_id: &str,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    let tmp = tmp_path(dst_root, cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    // lstat: a leftover dangling symlink at the tmp path reports exists()=false
    // but must still be removed, or the copy writes through the link target.
    if tmp.exists() || fs::symlink_metadata(&tmp).is_ok() {
        remove_any(&tmp)?;
    }
    copy_file_with_progress(src, dst_root, destination_id, entry, &tmp)
        .with_context(|| format!("failed to copy {} to {}", src.display(), tmp.display()))?;
    if let Some(expected_hash) = &entry.hash {
        let actual_hash = hash_file(&tmp)?;
        if &actual_hash != expected_hash {
            remove_any(&tmp).ok();
            bail!("source changed while copying {}", entry.rel_path);
        }
    }
    // Catch torn copies: a same-size mutation mid-read is invisible to the
    // size checks (and, without checksum mode, to any hash).
    if let Err(err) = ensure_source_stable(src, entry) {
        remove_any(&tmp).ok();
        return Err(err);
    }
    // fsync data before tightening mode (a read-only mode would block the
    // writable handle fsync needs on Windows).
    fsync_file(&tmp).with_context(|| format!("failed to fsync {}", entry.rel_path))?;
    set_mode(&tmp, entry.mode).ok();
    let mtime = FileTime::from_unix_time(
        entry.mtime_ns / 1_000_000_000,
        (entry.mtime_ns % 1_000_000_000) as u32,
    );
    if let Err(err) = set_file_mtime(&tmp, mtime) {
        // A file whose mtime cannot be recorded will compare as changed and
        // re-transfer every cycle; make that visible instead of silent.
        warn!(rel_path = entry.rel_path, error = %err, "failed to set file mtime");
    }
    replace_path(dst_root, cycle_id, &entry.rel_path, &tmp, final_path)?;
    fsync_parent(final_path).ok();
    Ok(())
}

/// Buffer for the streaming copy fallback. Kept modest (vs `TRANSFER_CHUNK_SIZE`)
/// so a pool of parallel local copies does not balloon resident memory.
const LOCAL_COPY_BUF: usize = 1024 * 1024;

fn copy_file_with_progress(
    src: &Path,
    _dst_root: &Path,
    _destination_id: &str,
    entry: &SnapshotEntry,
    tmp: &Path,
) -> Result<()> {
    copy_file_data(src, tmp, &entry.rel_path)
        .with_context(|| format!("failed to copy data {} -> {}", src.display(), tmp.display()))?;
    Ok(())
}

/// Copy file contents from `src` to a fresh `dst` at near system-`cp` speed.
/// Linux uses reflink (instant on ZFS 2.2+/btrfs) then `copy_file_range`
/// (kernel-space copy) before falling back to a streaming loop; Windows uses
/// `std::fs::copy` (`CopyFileExW`). Bytes moved are reported to the active
/// aggregate transfer meter via [`progress::record_transfer`].
fn copy_file_data(src: &Path, dst: &Path, rel_path: &str) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        if linux_fast_copy(src, dst, rel_path)? {
            return Ok(());
        }
    }
    #[cfg(windows)]
    {
        // CopyFileExW: cache-optimized, server-side copy over SMB.
        let copied = fs::copy(src, dst)?;
        progress::record_transfer(rel_path, copied);
        return Ok(());
    }
    #[allow(unreachable_code)]
    stream_copy(src, dst, rel_path)
}

/// Streaming read/write fallback with a bounded buffer.
fn stream_copy(src: &Path, dst: &Path, rel_path: &str) -> io::Result<()> {
    let mut reader = File::open(src)?;
    let mut writer = File::create(dst)?;
    let mut buf = vec![0_u8; LOCAL_COPY_BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        progress::record_transfer(rel_path, n as u64);
    }
    writer.flush()?;
    Ok(())
}

/// Returns `Ok(true)` when the copy completed via reflink/`copy_file_range`,
/// `Ok(false)` when the kernel does not support those on this pair (caller
/// should stream), or `Err` on a genuine I/O error.
#[cfg(target_os = "linux")]
fn linux_fast_copy(src: &Path, dst: &Path, rel_path: &str) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;

    // FICLONE = _IOW(0x94, 9, int) on the asm-generic ioctl layout (x86_64/aarch64).
    const FICLONE: libc::c_ulong = 0x4004_9409;

    let src_file = File::open(src)?;
    let dst_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;
    let src_fd = src_file.as_raw_fd();
    let dst_fd = dst_file.as_raw_fd();
    let total = src_file.metadata()?.len();

    // 1) reflink — block-level clone, no data movement.
    if unsafe { libc::ioctl(dst_fd, FICLONE, src_fd) } == 0 {
        progress::record_transfer(rel_path, total);
        return Ok(true);
    }

    // 2) copy_file_range — kernel-space copy, no userspace bounce buffer.
    let mut remaining = total;
    let mut any = false;
    while remaining > 0 {
        let n = unsafe {
            libc::copy_file_range(
                src_fd,
                std::ptr::null_mut(),
                dst_fd,
                std::ptr::null_mut(),
                remaining as usize,
                0,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            let raw = err.raw_os_error().unwrap_or(0);
            // Unsupported for this fs pair: only fall back if we have not yet
            // copied anything (otherwise dst is half-written and we error out).
            if !any
                && matches!(
                    raw,
                    libc::ENOSYS | libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL | libc::EBADF
                )
            {
                drop(dst_file);
                return Ok(false);
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        any = true;
        remaining -= n as u64;
        progress::record_transfer(rel_path, n as u64);
    }
    Ok(true)
}

fn copy_symlink(
    src: &Path,
    dst_root: &Path,
    cycle_id: i64,
    entry: &SnapshotEntry,
    final_path: &Path,
) -> Result<()> {
    let target =
        fs::read_link(src).with_context(|| format!("failed to read symlink {}", src.display()))?;
    let tmp = tmp_path(dst_root, cycle_id, &entry.rel_path);
    if let Some(parent) = tmp.parent() {
        fs::create_dir_all(parent)?;
    }
    // lstat: a leftover dangling symlink at the tmp path reports exists()=false
    // but must still be removed, or the copy writes through the link target.
    if tmp.exists() || fs::symlink_metadata(&tmp).is_ok() {
        remove_any(&tmp)?;
    }
    create_symlink_kind(&target, &tmp, symlink_points_to_dir(src))
        .with_context(|| format!("failed to create symlink {}", tmp.display()))?;
    if Some(hash_symlink(&tmp)?) != entry.hash {
        remove_any(&tmp).ok();
        bail!("source symlink changed while copying {}", entry.rel_path);
    }
    replace_path(dst_root, cycle_id, &entry.rel_path, &tmp, final_path)?;
    fsync_parent(final_path).ok();
    Ok(())
}

fn replace_path(
    dst_root: &Path,
    cycle_id: i64,
    rel: &str,
    tmp: &Path,
    final_path: &Path,
) -> Result<()> {
    if final_path.exists() || fs::symlink_metadata(final_path).is_ok() {
        let tmp_meta = fs::symlink_metadata(tmp)?;
        let final_meta = fs::symlink_metadata(final_path)?;
        let compatible = (tmp_meta.is_file() && final_meta.is_file())
            || (tmp_meta.file_type().is_symlink() && final_meta.file_type().is_symlink());
        if !compatible {
            // A type flip can replace a whole directory tree; it belongs in
            // the recycle bin like every other destructive replacement.
            move_to_trash(dst_root, rel, cycle_id)?;
        }
    }
    fs::rename(tmp, final_path).with_context(|| {
        format!(
            "failed to rename {} to {}",
            tmp.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

fn move_to_trash(dst_root: &Path, rel: &str, cycle_id: i64) -> Result<()> {
    let path = dst_root.join(rel);
    if !path.exists() && fs::symlink_metadata(&path).is_err() {
        return Ok(());
    }
    let trash = dst_root
        .join(INTERNAL_TRASH)
        .join(cycle_id.to_string())
        .join(rel);
    if let Some(parent) = trash.parent() {
        fs::create_dir_all(parent)?;
    }
    if trash.exists() || fs::symlink_metadata(&trash).is_ok() {
        remove_any(&trash)?;
    }
    match fs::rename(&path, &trash) {
        Ok(()) => Ok(()),
        // Vanished between the existence check and the rename — some other
        // pass already removed it, which is the goal state.
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        // Nested mounts can't rename across the device boundary; keep the
        // recycle-bin promise with a copy into the trash before deleting.
        Err(err) if err.kind() == io::ErrorKind::CrossesDevices => {
            copy_tree_best_effort(&path, &trash)
                .with_context(|| format!("failed to copy {rel} into the trash across devices"))?;
            remove_any(&path)?;
            Ok(())
        }
        // Anything else must fail loudly: silently degrading to a permanent
        // recursive delete erased recycle-bin contents in the past.
        Err(err) => Err(err).with_context(|| {
            format!(
                "failed to move {} into the trash at {}",
                path.display(),
                trash.display()
            )
        }),
    }
}

/// Recursive copy used only for cross-device trash moves: preserves files,
/// symlinks and directory structure (modes/mtimes are not needed for trash).
fn copy_tree_best_effort(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(from)?;
        create_symlink_kind(&target, to, symlink_points_to_dir(from))?;
        return Ok(());
    }
    if meta.is_file() {
        fs::copy(from, to)?;
        return Ok(());
    }
    fs::create_dir_all(to)?;
    for child in fs::read_dir(from)? {
        let child = child?;
        copy_tree_best_effort(&child.path(), &to.join(child.file_name()))?;
    }
    Ok(())
}

/// Re-check just the entries this cycle wrote (per-path lstat, plus re-hash in
/// checksum mode) instead of re-walking the whole destination tree. Untouched
/// entries were already compared against the fresh destination scan at the
/// start of the cycle, and mirror removals propagate their own errors, so a
/// full end-of-cycle re-scan only repeats work — on multi-terabyte trees it
/// used to double the cycle cost.
fn verify_copied_entries<'a, I>(
    dst_root: &Path,
    copied: I,
    ignored_paths: &BTreeSet<String>,
    sync: &NativeSyncConfig,
) -> Result<()>
where
    I: IntoIterator<Item = &'a SnapshotEntry>,
{
    set_sync_phase("verifying");
    for want in copied {
        if want.file_type == "dir" || ignored_paths.contains(&want.rel_path) {
            continue;
        }
        let path = safe_join_rel(dst_root, &want.rel_path)?;
        if fs::symlink_metadata(&path).is_err() {
            bail!("destination missing {}", want.rel_path);
        }
        let got = snapshot_entry(&path, want.rel_path.clone(), sync.checksum)?;
        // Exact: the copy stamped the destination with the source's mtime.
        if !entries_match_exact(want, &got, sync) {
            bail!("destination mismatch at {}", want.rel_path);
        }
    }
    Ok(())
}

fn entries_match(left: &SnapshotEntry, right: &SnapshotEntry, sync: &NativeSyncConfig) -> bool {
    entries_match_with(left, right, sync, false)
}

/// Exact-mtime variant for paths carrying positive change evidence (watcher
/// events, `zfs diff` output). The modify window exists to absorb filesystem
/// timestamp granularity during whole-tree quick checks; on an evidence path
/// it would swallow a same-size rewrite landing within the window of the
/// previously synced version and leave the drift green forever.
fn entries_match_exact(
    left: &SnapshotEntry,
    right: &SnapshotEntry,
    sync: &NativeSyncConfig,
) -> bool {
    entries_match_with(left, right, sync, true)
}

fn entries_match_with(
    left: &SnapshotEntry,
    right: &SnapshotEntry,
    sync: &NativeSyncConfig,
    exact: bool,
) -> bool {
    if left.file_type != right.file_type {
        return false;
    }
    match left.file_type.as_str() {
        "dir" => mtimes_match(left.mtime_ns, right.mtime_ns, sync),
        // Hash evidence wins whenever both sides carry one: checksum mode, or
        // a targeted hash upgrade of suspicious paths (dst-side-only diffs).
        "file" if sync.checksum || (left.hash.is_some() && right.hash.is_some()) => {
            left.size == right.size && left.hash == right.hash
        }
        "file" if exact => {
            left.size == right.size && mtimes_match_exact(left.mtime_ns, right.mtime_ns)
        }
        "file" => left.size == right.size && mtimes_match(left.mtime_ns, right.mtime_ns, sync),
        "symlink" => left.hash == right.hash,
        _ => false,
    }
}

/// Content-equal file entries whose permission bits differ are repaired with
/// a chmod in place instead of a re-copy (owner/group are not tracked).
fn entry_mode_differs(left: &SnapshotEntry, right: &SnapshotEntry) -> bool {
    left.file_type == "file" && left.mode != right.mode
}

fn should_attempt_delta(source: &SnapshotEntry, existing: &SnapshotEntry) -> bool {
    if source.file_type != "file" || existing.file_type != "file" || source.hash.is_some() {
        return false;
    }
    let source_size = source.size.max(0) as u64;
    let existing_size = existing.size.max(0) as u64;
    if !(DELTA_MIN_SIZE..=DELTA_MAX_SIZE).contains(&source_size) || existing_size == 0 {
        return false;
    }
    if source_size == existing_size {
        return false;
    }
    let smaller = source_size.min(existing_size);
    let larger = source_size.max(existing_size);
    smaller.saturating_mul(10) >= larger.saturating_mul(7)
}

fn mtimes_match(left_ns: i64, right_ns: i64, sync: &NativeSyncConfig) -> bool {
    let window_ns = (sync.modify_window_secs as i128) * 1_000_000_000;
    (left_ns as i128 - right_ns as i128).abs() <= window_ns
}

/// Windows FILETIME stores 100ns ticks, so a nanosecond mtime applied on an
/// NTFS destination reads back truncated; tolerate that but nothing coarser —
/// a genuine rewrite lands whole milliseconds-to-seconds away.
const EXACT_MTIME_TOLERANCE_NS: i128 = 1_000;

fn mtimes_match_exact(left_ns: i64, right_ns: i64) -> bool {
    (left_ns as i128 - right_ns as i128).abs() <= EXACT_MTIME_TOLERANCE_NS
}

/// After reading a source file's content, confirm the file is still exactly
/// the version its snapshot entry described (size AND mtime). A concurrent
/// writer can mutate the file mid-read without changing its size, producing a
/// TORN copy (half old, half new) whose own transfer hash still checks out —
/// the hash covers what was read, not a consistent version. Reporting the
/// canonical source-changing error makes the cycle record a yellow issue and
/// re-copy the settled file next round. Snapshot read views (ZFS) are
/// immutable, so there this is a cheap no-op lstat.
fn ensure_source_stable(src: &Path, entry: &SnapshotEntry) -> Result<()> {
    let metadata = fs::symlink_metadata(src)
        .with_context(|| format!("failed to re-check source {}", src.display()))?;
    let mtime_ns = metadata_mtime_ns(&metadata)?;
    if metadata.len() as i64 != entry.size || mtime_ns != entry.mtime_ns {
        warn!(
            rel_path = entry.rel_path,
            snapshot_size = entry.size,
            live_size = metadata.len(),
            snapshot_mtime_ns = entry.mtime_ns,
            live_mtime_ns = mtime_ns,
            "source changed while its content was being copied"
        );
        bail!("source changed while copying {}", entry.rel_path);
    }
    Ok(())
}

/// Relative-path index over a snapshot for read-only comparisons; borrows
/// instead of deep-copying every entry (snapshots run to ~600K entries).
fn map_entry_refs(entries: &[SnapshotEntry]) -> BTreeMap<&str, &SnapshotEntry> {
    entries
        .iter()
        .map(|entry| (entry.rel_path.as_str(), entry))
        .collect()
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0_u8; 1024 * 64];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn hash_symlink(path: &Path) -> Result<String> {
    let target = fs::read_link(path)?;
    Ok(hash_symlink_target(&target))
}

/// Symlink fingerprint. Windows normalizes separators to `/` so a relative
/// target compares equal to the Unix copy the wire conversion produces; Unix
/// keeps the raw (lossy) string — `\` is a legal target byte there.
fn hash_symlink_target(target: &Path) -> String {
    #[cfg(windows)]
    {
        format!("symlink:{}", target.to_string_lossy().replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        format!("symlink:{}", target.to_string_lossy())
    }
}

fn tmp_path(dst_root: &Path, cycle_id: i64, rel: &str) -> PathBuf {
    let file_name = Path::new(rel)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "entry".to_string());
    let parent = Path::new(rel).parent().unwrap_or_else(|| Path::new(""));
    dst_root
        .join(INTERNAL_TMP)
        .join(cycle_id.to_string())
        .join(parent)
        .join(format!("{file_name}.tmp.{}", std::process::id()))
}

/// lstat check: does something sit at `path` that is not a real directory?
/// A symlink counts even when it points at a directory — writing "into" it
/// would land the subtree at the link target instead of the mirror path.
fn path_blocks_directory(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(meta) => meta.file_type().is_symlink() || !meta.is_dir(),
        Err(_) => false,
    }
}

fn remove_any(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        // Windows directory symlinks/junctions are directory handles:
        // remove_file returns ACCESS_DENIED for them and they need
        // RemoveDirectory. Never remove_dir_all here — that would recurse
        // into the link target.
        if fs::remove_file(path).is_err() {
            fs::remove_dir(path)
                .with_context(|| format!("failed to remove symlink {}", path.display()))?;
        }
        return Ok(());
    }
    if meta.is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    let mut perms = fs::symlink_metadata(path)?.permissions();
    set_permissions_mode(&mut perms, mode);
    fs::set_permissions(path, perms)?;
    Ok(())
}

fn metadata_mtime_ns(metadata: &fs::Metadata) -> Result<i64> {
    let modified = metadata.modified()?;
    let ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let secs = i64::try_from(duration.as_secs()).context("mtime seconds overflow")?;
            secs.checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(i64::from(duration.subsec_nanos())))
                .ok_or_else(|| anyhow!("mtime nanoseconds overflow"))?
        }
        Err(err) => {
            let duration = err.duration();
            let secs = i64::try_from(duration.as_secs()).context("mtime seconds overflow")?;
            let ns = secs
                .checked_mul(1_000_000_000)
                .and_then(|value| value.checked_add(i64::from(duration.subsec_nanos())))
                .ok_or_else(|| anyhow!("mtime nanoseconds overflow"))?;
            -ns
        }
    };
    Ok(ns)
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    metadata.mode()
}

#[cfg(windows)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        0o120777
    } else if metadata.is_dir() {
        0o040755
    } else if metadata.permissions().readonly() {
        0o100444
    } else {
        0o100644
    }
}

#[cfg(unix)]
fn set_permissions_mode(perms: &mut fs::Permissions, mode: u32) {
    perms.set_mode(mode);
}

#[cfg(windows)]
fn set_permissions_mode(perms: &mut fs::Permissions, mode: u32) {
    perms.set_readonly(mode & 0o222 == 0);
}

/// Create a symlink at `tmp` pointing to `target`. `is_dir` says whether the
/// link points to a directory — required on Windows, which has distinct
/// directory- and file-symlink kinds. The source decides `is_dir` (it can see
/// the link's target); the destination cannot, since the link does not exist
/// there yet (this was the Linux-dir-symlink → Windows-file-symlink bug).
#[cfg(unix)]
fn create_symlink_kind(target: &Path, tmp: &Path, _is_dir: bool) -> io::Result<()> {
    symlink(target, tmp)
}

#[cfg(windows)]
fn create_symlink_kind(target: &Path, tmp: &Path, is_dir: bool) -> io::Result<()> {
    if is_dir {
        symlink_dir(target, tmp)
    } else {
        symlink_file(target, tmp)
    }
}

/// Whether the symlink at `src` resolves to a directory (follows the link).
/// Dangling links report `false` (best guess; the target type is unknowable).
fn symlink_points_to_dir(src: &Path) -> bool {
    fs::metadata(src).map(|meta| meta.is_dir()).unwrap_or(false)
}

/// Whether received files are fsync'd for durability. Off by default; an fsync
/// per file is the dominant cost on sync filesystems like ZFS (see `fsync` in
/// [`NativeSyncConfig`]). Set per process from config via [`configure_fsync`].
static FSYNC_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn configure_fsync(enabled: bool) {
    FSYNC_ENABLED.store(enabled, Ordering::Relaxed);
}

fn fsync_enabled() -> bool {
    FSYNC_ENABLED.load(Ordering::Relaxed)
}

fn fsync_file(path: &Path) -> io::Result<()> {
    if !fsync_enabled() {
        return Ok(());
    }
    // FlushFileBuffers on Windows (and durability semantics generally) needs a
    // writable handle, so open for write rather than read. A read-only attribute
    // (e.g. copied from a read-only source) would block that open on Windows, so
    // clear it first; the caller applies the final mode afterwards.
    #[cfg(windows)]
    {
        let mut perms = fs::metadata(path)?.permissions();
        if perms.readonly() {
            perms.set_readonly(false);
            fs::set_permissions(path, perms)?;
        }
    }
    OpenOptions::new().write(true).open(path)?.sync_all()
}

fn fsync_parent(path: &Path) -> io::Result<()> {
    if !fsync_enabled() {
        return Ok(());
    }
    // A directory handle cannot be opened as a writable File on Windows; the
    // file fsync plus the rename is the durability point there.
    #[cfg(not(windows))]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(windows)]
    let _ = path;
    Ok(())
}

fn rel_to_string(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => match part.to_str() {
                Some(value) => parts.push(value.to_string()),
                // A lossy conversion would sync (and verify against) a
                // DIFFERENT name than the file actually has — silently wrong
                // both ways. Callers that walk trees skip such entries with a
                // warning instead.
                None => bail!(
                    "path component is not valid UTF-8: {}",
                    path.to_string_lossy()
                ),
            },
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("invalid relative path: {}", path.display());
            }
        }
    }
    if parts.is_empty() {
        bail!("invalid empty relative path");
    }
    Ok(parts.join("/"))
}

fn file_name_string(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.display()))?;
    let value = name.to_string_lossy().to_string();
    if value.is_empty() {
        bail!("path has empty file name: {}", path.display());
    }
    Ok(value)
}

fn path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn short_reason(err: &anyhow::Error) -> String {
    let text = err
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    text.chars().take(120).collect()
}

fn standby_gate_blocks_sync(gate: &crate::core::standby::Gate, manual_full_rescan: bool) -> bool {
    match gate {
        crate::core::standby::Gate::Asleep { .. } => !manual_full_rescan,
        crate::core::standby::Gate::NotMounted { .. } => true,
    }
}

/// Record a destination sync failure: tolerated source-changing errors become
/// yellow `source_changing` issues (the next cycle converges them); everything
/// else goes red with a short reason.
/// If this pass left the cycle `failed` (a destination went red — offline,
/// dir-create failure, an unusable event plan, ...), return a human reason
/// assembled from the red destinations' statuses. `None` when the pass did not
/// fail. The task-finish logic uses this so a failed pass is always logged (as
/// `failed`) rather than discarded as a no-op when it transferred 0 files.
fn failed_pass_reason(state: &State, source: &SourceGroupConfig, cycle_id: i64) -> Option<String> {
    let status = state
        .cycle_by_id(&source.id, cycle_id)
        .ok()
        .flatten()?
        .status;
    if status != "failed" {
        return None;
    }
    let mut reasons: Vec<String> = Vec::new();
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if let Ok(offset) = state.destination_offset(&source.id, &dst.id) {
            if offset.status == "red" {
                let reason = if offset.status_reason.is_empty() {
                    "failed".to_string()
                } else {
                    offset.status_reason.clone()
                };
                reasons.push(format!("{}: {}", dst.id, reason));
            }
        }
    }
    Some(if reasons.is_empty() {
        "sync pass failed".to_string()
    } else {
        reasons.join("; ")
    })
}

/// When a pass left the cycle `failed` ONLY because the live source changed
/// under the copy (every affected destination is yellow `source_changing`, none
/// red), returns the number of affected destinations. Such a pass copied
/// everything that was stable and deferred just the changed paths to the next
/// cycle — it is a warning, not a failure, and must not be logged red.
fn source_changing_only_pass(
    state: &State,
    source: &SourceGroupConfig,
    cycle_id: i64,
) -> Option<usize> {
    let status = state
        .cycle_by_id(&source.id, cycle_id)
        .ok()
        .flatten()?
        .status;
    if status != "failed" {
        return None;
    }
    let mut changing = 0usize;
    for dst in source.destinations.iter().filter(|dst| dst.enabled) {
        if let Ok(offset) = state.destination_offset(&source.id, &dst.id) {
            if offset.status == "red" {
                return None; // a genuine failure is present — not a warning
            }
            if offset.status_reason == "source_changing" {
                changing += 1;
            }
        }
    }
    (changing > 0).then_some(changing)
}

fn record_destination_failure(
    state: &State,
    source_id: &str,
    destination_id: &str,
    cycle_id: i64,
    err: &anyhow::Error,
) -> Result<()> {
    if cancel::error_is_cancelled(err) {
        // Cancelled means STOP: drop the manual Full/Changed-Since flags and
        // the in-flight target so the scheduler does not immediately restart
        // the same heavy pass. The verified baseline stays; the destination
        // re-targets on its schedule, the next event, or a manual sync.
        // Event-loss repairs are NOT lost — the rescan_required event rows
        // still force a full reconcile once a new target is set.
        state.clear_cycle_needs_rescan(cycle_id)?;
        state.clear_destination_issues(source_id, destination_id)?;
        state.clear_destination_target(source_id, destination_id, "cancelled")?;
        return Ok(());
    }
    let changing_paths = source_changed_paths(err);
    if changing_paths.is_empty() {
        state.clear_destination_issues(source_id, destination_id)?;
        state.upsert_destination_status(
            source_id,
            destination_id,
            None,
            "red",
            &short_reason(err),
        )?;
    } else {
        // Plan A — tolerate a live source that changed under the copy. The
        // source is non-snapshot (a ZFS source is read from an immutable
        // snapshot and never reaches here): everything stable was synced, only
        // these paths changed mid-read. ADVANCE the verified offset to this
        // cycle so the scheduler stops re-running a full reconcile forever
        // against a source that never goes quiet (e.g. an app directory written
        // continuously) — without this, verified stays below target, the ready
        // check never skips the destination, and every wake re-walks the whole
        // tree. Re-record the changed paths as source events so the next cycle
        // (its schedule, or realtime) syncs just those paths instead of the
        // whole tree.
        for path in &changing_paths {
            state.record_event(source_id, 0, "source_changing_deferred", Some(path), false)?;
        }
        state.replace_destination_issues(
            source_id,
            destination_id,
            cycle_id,
            "source_changing",
            &changing_paths,
            "source file changed while copying; deferred to the next cycle",
        )?;
        state.upsert_destination_status(
            source_id,
            destination_id,
            Some(cycle_id),
            "yellow",
            "source_changing",
        )?;
    }
    Ok(())
}

fn source_changed_paths(err: &anyhow::Error) -> Vec<String> {
    let mut paths = BTreeSet::new();
    let prefixes = [
        "source changed while copying ",
        "source symlink changed while copying ",
    ];
    for cause in err.chain() {
        let text = cause.to_string();
        for line in text.lines() {
            for prefix in prefixes {
                if let Some((_, path)) = line.split_once(prefix) {
                    let path = path.trim();
                    if !path.is_empty() {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
    paths.into_iter().collect()
}

fn source_changing_error(paths: &BTreeSet<String>) -> anyhow::Error {
    anyhow!(
        "{}",
        paths
            .iter()
            .map(|path| format!("source changed while copying {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Stale partial transfers from cycles whose target has moved on: kept this
/// long to allow same-cycle resume across restarts, then reclaimed.
const TMP_KEEP_DAYS: u64 = 7;

fn cleanup_tmp_cycle(dst_root: &Path, cycle_id: i64) {
    let root = dst_root.join(INTERNAL_TMP);
    let path = root.join(cycle_id.to_string());
    if path.exists() {
        remove_any(&path).ok();
    }
    // Other cycles' tmp dirs: needed only while their own cycle can still
    // resume; once abandoned (cancel + new cycle, manual full) nothing ever
    // cleaned them and multi-GB partial files accumulated forever. Age-based
    // so a concurrent pass for another source on the same root is never hit.
    sweep_aged_subdirs(&root, TMP_KEEP_DAYS, Some(cycle_id));
    fs::remove_dir(&root).ok();
}

/// Reclaim `.auto_sync_trash/<cycle>` folders whose last write is older than
/// the retention window. 0 = keep forever.
fn cleanup_expired_trash(dst_root: &Path, trash_keep_days: u64) {
    if trash_keep_days == 0 {
        return;
    }
    let root = dst_root.join(INTERNAL_TRASH);
    sweep_aged_subdirs(&root, trash_keep_days, None);
    fs::remove_dir(&root).ok();
}

fn sweep_aged_subdirs(root: &Path, keep_days: u64, keep_name: Option<i64>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(Duration::from_secs(keep_days.saturating_mul(24 * 3600)))
        .unwrap_or(UNIX_EPOCH);
    for entry in entries.filter_map(|e| e.ok()) {
        if let Some(keep) = keep_name {
            if entry.file_name().to_string_lossy() == keep.to_string() {
                continue;
            }
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let modified = meta.modified().unwrap_or(UNIX_EPOCH);
        if modified < cutoff {
            remove_any(&entry.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::ScheduleMode;
    use crate::core::config::{
        AppConfig, DestinationConfig, MachineConfig, ScheduleConfig, SnapshotBackend,
        SnapshotConfig, SourceGroupConfig, SyncMode,
    };
    use crate::core::state::State;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn extracts_file_name_from_windows_paths() {
        assert_eq!(
            cross_platform_file_name(Path::new("C:\\Users\\me\\blog")),
            Some("blog".to_string())
        );
        assert_eq!(
            cross_platform_file_name(Path::new("D:\\data\\source\\")),
            Some("source".to_string())
        );
        assert_eq!(cross_platform_file_name(Path::new("C:\\")), None);
    }

    #[test]
    fn joins_paths_using_target_machine_separator() {
        let mut linux = MachineConfig::local();
        linux.os = "linux".to_string();
        assert_eq!(
            join_machine_path(Path::new("/zfs/tmp"), "auto_sync_test", &linux).to_string_lossy(),
            "/zfs/tmp/auto_sync_test"
        );

        let mut windows = MachineConfig::local();
        windows.os = "windows".to_string();
        assert_eq!(
            join_machine_path(Path::new("C:\\Users\\tiger"), "auto_sync_test", &windows)
                .to_string_lossy(),
            "C:\\Users\\tiger\\auto_sync_test"
        );
    }

    #[test]
    fn directory_destination_root_can_flatten_or_add_source_directory() {
        let mut linux = MachineConfig::local();
        linux.os = "linux".to_string();
        let source_info = TransferPathInfo {
            kind: "dir".to_string(),
            base: PathBuf::from("/zfs"),
            name: "zfs".to_string(),
        };
        let mut source = SourceGroupConfig {
            add_directory: false,
            managed_by: String::new(),
            ..SourceGroupConfig::default()
        };

        assert_eq!(
            destination_root_for_source(&source, &source_info, Path::new("/zfs_pool"), &linux),
            PathBuf::from("/zfs_pool")
        );

        source.add_directory = true;
        assert_eq!(
            destination_root_for_source(&source, &source_info, Path::new("/zfs_pool"), &linux),
            PathBuf::from("/zfs_pool/zfs")
        );
    }

    #[test]
    fn relative_paths_are_normalized_for_cross_platform_transfer() {
        assert_eq!(
            rel_to_string(Path::new("nested\\child.txt")).unwrap(),
            "nested/child.txt"
        );
        assert_eq!(
            safe_join_rel(Path::new("/tmp/root"), "nested\\child.txt")
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/"),
            "/tmp/root/nested/child.txt"
        );
        assert!(safe_join_rel(Path::new("/tmp/root"), "C:\\escape.txt").is_err());
    }

    #[test]
    fn parses_destination_sync_request_modes() {
        assert_eq!(
            "incremental".parse::<SyncRequestMode>().unwrap(),
            SyncRequestMode::Incremental
        );
        assert_eq!(
            "full".parse::<SyncRequestMode>().unwrap(),
            SyncRequestMode::Full
        );
        // Changed Since was removed: its wire values are now rejected.
        assert!("changed_since".parse::<SyncRequestMode>().is_err());
        assert!("other".parse::<SyncRequestMode>().is_err());
    }

    #[test]
    fn transfer_receive_file_chunk_path_encodes_windows_paths() {
        let entry = SnapshotEntry {
            rel_path: "dir/hello world.txt".to_string(),
            file_type: "file".to_string(),
            size: 5,
            mtime_ns: 123,
            mode: 0o644,
            hash: Some("abc+123".to_string()),
        };

        let path = receive_file_chunk_api_path(Path::new("C:\\sync root"), 42, &entry, 7);

        assert!(path.starts_with("/api/transfer/receive-file-chunk?"));
        assert!(path.contains("root=C%3A%5Csync%20root"));
        assert!(path.contains("rel_path=dir%2Fhello%20world.txt"));
        assert!(path.contains("offset=7"));
    }

    #[test]
    fn performance_strategy_streams_large_same_size_rewrites() {
        assert!(TRANSFER_CHUNK_SIZE >= 16 * 1024 * 1024);

        let source = test_file_entry("video.mp4", 512 * 1024 * 1024);
        let existing = test_file_entry("video.mp4", 512 * 1024 * 1024);

        assert!(!should_attempt_delta(&source, &existing));
    }

    #[test]
    fn performance_strategy_keeps_delta_for_append_like_files() {
        let source = test_file_entry("archive.log", 512 * 1024 * 1024);
        let existing = test_file_entry("archive.log", 480 * 1024 * 1024);

        assert!(should_attempt_delta(&source, &existing));
    }

    #[test]
    fn transfer_safe_join_rejects_absolute_and_parent_paths() {
        let root = Path::new("/tmp/root");

        assert_eq!(
            safe_join_rel(root, "nested/file.txt").unwrap(),
            root.join("nested").join("file.txt")
        );
        assert!(safe_join_rel(root, "../escape.txt").is_err());
        assert!(safe_join_rel(root, "/escape.txt").is_err());
    }

    #[test]
    fn chunked_transfer_resumes_and_finishes_file() {
        let temp = temp_dir("chunked_transfer_resume");
        let root = temp.join("dst");
        let bytes = b"hello chunked resume";
        let hash = blake3::hash(bytes).to_hex().to_string();
        let entry = SnapshotEntry {
            rel_path: "nested/hello.txt".to_string(),
            file_type: "file".to_string(),
            size: bytes.len() as i64,
            mtime_ns: 123,
            mode: 0o644,
            hash: Some(hash),
        };

        transfer_receive_file_chunk(
            TransferReceiveFileChunkQuery {
                root: root.to_string_lossy().to_string(),
                rel_path: entry.rel_path.clone(),
                cycle_id: 11,
                size: entry.size,
                offset: 0,
            },
            &bytes[..6],
        )
        .unwrap();
        let offset = transfer_file_offset(TransferFileOffsetRequest {
            root: root.clone(),
            rel_path: entry.rel_path.clone(),
            cycle_id: 11,
            size: entry.size,
        })
        .unwrap()
        .offset;
        assert_eq!(offset, 6);

        transfer_receive_file_chunk(
            TransferReceiveFileChunkQuery {
                root: root.to_string_lossy().to_string(),
                rel_path: entry.rel_path.clone(),
                cycle_id: 11,
                size: entry.size,
                offset,
            },
            &bytes[offset as usize..],
        )
        .unwrap();
        transfer_finish_file(TransferFinishFileRequest {
            root: root.clone(),
            cycle_id: 11,
            entry,
            full_hash: Some(blake3::hash(&bytes[..]).to_hex().to_string()),
        })
        .unwrap();

        assert_eq!(
            fs::read(root.join("nested").join("hello.txt")).unwrap(),
            bytes
        );
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn syncs_source_file_to_destination_directory() {
        let temp = temp_dir("file_to_dir");
        let src = temp.join("source.txt");
        let dst = temp.join("dst");
        fs::create_dir_all(&dst).unwrap();
        fs::write(&src, b"hello").unwrap();

        sync_endpoint(
            &SourceEndpoint::File { path: src.clone() },
            &DestinationEndpoint::Dir { root: dst.clone() },
            "test_dst",
            7,
            &[],
            &[],
            &NativeSyncConfig::default(),
        )
        .unwrap();

        assert_eq!(fs::read(dst.join("source.txt")).unwrap(), b"hello");
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn syncs_source_file_to_destination_file() {
        let temp = temp_dir("file_to_file");
        let src = temp.join("source.txt");
        let dst = temp.join("renamed.txt");
        fs::write(&src, b"hello").unwrap();
        fs::write(&dst, b"old").unwrap();

        sync_endpoint(
            &SourceEndpoint::File { path: src.clone() },
            &DestinationEndpoint::File { path: dst.clone() },
            "test_dst",
            7,
            &[],
            &[],
            &NativeSyncConfig::default(),
        )
        .unwrap();

        assert_eq!(fs::read(dst).unwrap(), b"hello");
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn syncs_directory_mtime_after_children() {
        let temp = temp_dir("dir_mtime");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let parent = src.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(&child).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(child.join("file.txt"), b"hello").unwrap();
        let parent_time = FileTime::from_unix_time(1_700_000_100, 0);
        let child_time = FileTime::from_unix_time(1_700_000_200, 0);
        set_file_mtime(&parent, parent_time).unwrap();
        set_file_mtime(&child, child_time).unwrap();

        let source_snapshot =
            take_snapshot_with_excludes(&src, SnapshotMode::Source, &[], false).unwrap();
        sync_endpoint(
            &SourceEndpoint::Dir {
                root: src.clone(),
                add_directory: true,
            },
            &DestinationEndpoint::Dir { root: dst.clone() },
            "test_dst",
            7,
            &source_snapshot,
            &[],
            &NativeSyncConfig::default(),
        )
        .unwrap();

        let parent_entry = source_snapshot
            .iter()
            .find(|entry| entry.rel_path == "parent")
            .unwrap();
        let child_entry = source_snapshot
            .iter()
            .find(|entry| entry.rel_path == "parent/child")
            .unwrap();
        let parent_mtime = metadata_mtime_ns(&fs::metadata(dst.join("parent")).unwrap()).unwrap();
        let child_mtime =
            metadata_mtime_ns(&fs::metadata(dst.join("parent").join("child")).unwrap()).unwrap();
        assert!(mtimes_match(
            parent_entry.mtime_ns,
            parent_mtime,
            &NativeSyncConfig::default()
        ));
        assert!(mtimes_match(
            child_entry.mtime_ns,
            child_mtime,
            &NativeSyncConfig::default()
        ));
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn snapshot_scans_breadth_first() {
        let temp = temp_dir("snapshot_breadth_first");
        let src = temp.join("src");
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("b.txt"), b"b").unwrap();
        fs::write(src.join("a").join("deep.txt"), b"deep").unwrap();

        let snapshot = take_snapshot_with_excludes(&src, SnapshotMode::Source, &[], false).unwrap();
        let paths: Vec<String> = snapshot.into_iter().map(|entry| entry.rel_path).collect();

        assert_eq!(paths, vec!["a", "b.txt", "a/deep.txt"]);
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn rel_excludes_support_segment_glob_and_plain_prefix() {
        let excludes = vec![
            PathBuf::from("*/cache"),
            PathBuf::from("*/temp"),
            PathBuf::from("Backup/old"),
        ];
        // `*/cache` matches any account's cache dir and its subtree...
        assert!(is_rel_excluded(Path::new("x774796_4d47/cache"), &excludes));
        assert!(is_rel_excluded(
            Path::new("wxid_abc/cache/img/1.dat"),
            &excludes
        ));
        assert!(is_rel_excluded(Path::new("wxid_abc/temp/x"), &excludes));
        // ...but not the precious dirs next to it.
        assert!(!is_rel_excluded(
            Path::new("x774796_4d47/db_storage/c.db"),
            &excludes
        ));
        assert!(!is_rel_excluded(Path::new("x774796_4d47/msg/a"), &excludes));
        // A top-level `cache` (no account segment) is NOT matched by `*/cache`.
        assert!(!is_rel_excluded(Path::new("cache"), &excludes));
        // Plain (non-glob) prefix still works.
        assert!(is_rel_excluded(Path::new("Backup/old/f"), &excludes));
        assert!(!is_rel_excluded(Path::new("Backup/new/f"), &excludes));
    }

    #[test]
    fn source_walk_prunes_auto_sync_internal_dirs() {
        // A source that is itself another mapping's destination accumulates
        // `.auto_sync_trash`. It must never enter a source snapshot, or every
        // trash entry becomes a `to_add` the destination lacks and no sync can
        // clear (the real /zfs → /zfs_pool "14147 phantom differences" bug).
        let temp = temp_dir("source_walk_prunes_internal");
        let src = temp.join("src");
        fs::create_dir_all(src.join("keep")).unwrap();
        fs::write(src.join("keep").join("real.txt"), b"real").unwrap();
        fs::create_dir_all(src.join(INTERNAL_TRASH).join("57")).unwrap();
        fs::write(src.join(INTERNAL_TRASH).join("57").join("gone.jpg"), b"x").unwrap();
        fs::create_dir_all(src.join(INTERNAL_TMP)).unwrap();
        fs::write(src.join(INTERNAL_PROBE), b"p").unwrap();

        let snapshot = take_snapshot_with_excludes(&src, SnapshotMode::Source, &[], false).unwrap();
        let paths: Vec<String> = snapshot.into_iter().map(|entry| entry.rel_path).collect();

        assert_eq!(paths, vec!["keep", "keep/real.txt"]);
        assert!(
            !paths.iter().any(|p| p.starts_with(".auto_sync")),
            "internal dirs leaked into source snapshot: {paths:?}"
        );
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn cancelled_operation_aborts_tree_walk() {
        let temp = temp_dir("cancel_aborts_walk");
        let src = temp.join("src");
        fs::create_dir_all(src.join("a")).unwrap();
        fs::write(src.join("a").join("f.txt"), b"f").unwrap();

        // Unique kind: the registry is process-global and tests run in
        // parallel — cancelling "sync"/"compare" could hit other tests' ops.
        let _op = cancel::begin("test-cancel-walk");
        cancel::request(Some("test-cancel-walk"), None);
        let err = take_snapshot_with_excludes(&src, SnapshotMode::Source, &[], false).unwrap_err();
        assert!(cancel::error_is_cancelled(&err), "got: {err:#}");
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn zfs_diff_compare_requires_both_baselines() {
        let temp = temp_dir("zfs_compare_fallback");
        let db = temp.join("state.sqlite");
        let src = temp.join("src");
        fs::create_dir_all(&src).unwrap();
        let state = State::open(&db).unwrap();
        let source = SourceGroupConfig {
            id: "zc_src".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: Vec::new(),
        };
        // No baselines recorded -> the fast path declines (caller runs the
        // full walk) instead of erroring.
        let outcome = zfs_diff_compare(
            &state,
            &source,
            "zc_dst",
            &temp.join("dst"),
            &NativeSyncConfig::default(),
        )
        .unwrap();
        assert!(outcome.is_none());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn snapshot_purpose_maps_to_cancel_kinds() {
        assert_eq!(
            snapshot_request_cancel_kind("compare"),
            cancel::KIND_COMPARE
        );
        assert_eq!(snapshot_request_cancel_kind("sync"), cancel::KIND_SYNC);
        // Old senders carry no purpose: treated as sync work.
        assert_eq!(snapshot_request_cancel_kind(""), cancel::KIND_SYNC);
    }

    #[test]
    fn missing_destination_directory_copies_whole_subtree() {
        let temp = temp_dir("missing_dst_dir_fast_path");
        let src = temp.join("src");
        let dst = temp.join("dst");
        fs::create_dir_all(src.join("top").join("nested")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("top").join("nested").join("file.txt"), b"hello").unwrap();

        let (snapshot, touched) = sync_destination_fast_missing_dirs(
            &src,
            &dst,
            "dst_1",
            7,
            &[],
            &NativeSyncConfig::default(),
        )
        .unwrap();
        let paths: BTreeSet<String> = snapshot.into_iter().map(|entry| entry.rel_path).collect();

        assert!(paths.contains("top"));
        assert!(paths.contains("top/nested"));
        assert!(paths.contains("top/nested/file.txt"));
        // The wholesale-copied subtree counts as touched (baseline cross-check).
        assert!(touched.contains("top/nested/file.txt"));
        assert_eq!(
            fs::read(dst.join("top").join("nested").join("file.txt")).unwrap(),
            b"hello"
        );
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn rejects_directory_source_to_destination_file() {
        let temp = temp_dir("dir_to_file");
        let src = temp.join("src");
        let dst = temp.join("dst-file");
        fs::create_dir_all(&src).unwrap();
        fs::write(&dst, b"old").unwrap();

        let result = DestinationEndpoint::resolve(
            &SourceEndpoint::Dir {
                root: src,
                add_directory: true,
            },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            },
        );

        assert!(result.is_err());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn rejects_system_path_destination() {
        let result = DestinationEndpoint::resolve(
            &SourceEndpoint::Dir {
                root: PathBuf::from("/tmp/source"),
                add_directory: true,
            },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: PathBuf::from("/dev"),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            },
        );

        assert!(result.is_err());
    }

    #[test]
    fn treats_missing_destination_path_as_directory_for_source_file() {
        let temp = temp_dir("missing_dst_is_dir");
        let src = temp.join("source.txt");
        let dst = temp.join("new-dir");
        fs::write(&src, b"hello").unwrap();

        let endpoint = DestinationEndpoint::resolve(
            &SourceEndpoint::File { path: src },
            &DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst,
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            },
        )
        .unwrap();

        assert!(matches!(endpoint, DestinationEndpoint::Dir { .. }));
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_all_now_sets_target_and_verifies_destination() {
        let temp = temp_dir("sync_all_now");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();
        assert_eq!(
            fs::read(dst.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );

        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].status, "green");
        assert_eq!(views[0].target_cycle_id, views[0].last_verified_cycle_id);
        assert!(views[0].target_cycle_id.is_some());

        fs::remove_dir_all(temp).ok();
    }

    fn batch_entry(rel: &str, size: i64) -> SnapshotEntry {
        SnapshotEntry {
            rel_path: rel.to_string(),
            file_type: "file".to_string(),
            size,
            mtime_ns: 1_000_000_000,
            mode: 0o644,
            hash: None,
        }
    }

    #[test]
    fn plan_push_work_batches_small_files_and_keeps_the_rest_single() {
        let small_a = batch_entry("a.txt", 10);
        let small_b = batch_entry("b.txt", 10);
        let big = batch_entry("big.bin", (SMALL_BATCH_FILE_MAX + 1) as i64);
        let delta = batch_entry("delta.bin", 10);
        let lone = batch_entry("lone.txt", 10);

        // Two adjacent smalls batch; the big and the delta-eligible file stay
        // single work items.
        let entries = vec![
            (&small_a, false),
            (&small_b, false),
            (&big, false),
            (&delta, true),
        ];
        let work = plan_push_work(&entries);
        assert_eq!(work.len(), 3);
        assert!(matches!(&work[0], PushWork::Single(entry, false) if entry.rel_path == "big.bin"));
        assert!(matches!(&work[1], PushWork::Single(entry, true) if entry.rel_path == "delta.bin"));
        assert!(matches!(&work[2], PushWork::SmallBatch(batch) if batch.len() == 2));

        // A batch of one degenerates to a Single (no gain over put-file).
        let entries = vec![(&lone, false)];
        let work = plan_push_work(&entries);
        assert_eq!(work.len(), 1);
        assert!(matches!(&work[0], PushWork::Single(entry, false) if entry.rel_path == "lone.txt"));

        // The per-batch file cap splits an oversized run.
        let many: Vec<SnapshotEntry> = (0..SMALL_BATCH_MAX_FILES + 1)
            .map(|i| batch_entry(&format!("f{i}"), 1))
            .collect();
        let refs: Vec<(&SnapshotEntry, bool)> = many.iter().map(|entry| (entry, false)).collect();
        let work = plan_push_work(&refs);
        assert_eq!(work.len(), 2);
        assert!(
            matches!(&work[0], PushWork::SmallBatch(batch) if batch.len() == SMALL_BATCH_MAX_FILES)
        );
        assert!(matches!(&work[1], PushWork::Single(..)));
    }

    #[test]
    fn put_files_batch_roundtrips_and_reports_per_file_failures() {
        let temp = temp_dir("put_files_batch");
        let root = temp.join("dst");
        fs::create_dir_all(&root).unwrap();

        let mut body: Vec<u8> = Vec::new();
        let mut frame = |rel: &str, payload: &[u8], hash: &str| {
            let header = BatchFileHeader {
                rel_path: rel.to_string(),
                size: payload.len() as i64,
                mtime_ns: 1_700_000_000_000_000_000,
                mode: 0o644,
                full_hash: hash.to_string(),
            };
            body.extend_from_slice(&serde_json::to_vec(&header).unwrap());
            body.push(b'\n');
            body.extend_from_slice(payload);
        };
        let good = b"hello batch";
        frame(
            "dir/good.txt",
            good,
            &blake3::hash(good).to_hex().to_string(),
        );
        // Wrong hash: must fail THAT file only, not the batch.
        frame(
            "bad.txt",
            b"corrupted",
            &blake3::hash(b"other").to_hex().to_string(),
        );
        let also = b"second good";
        frame("also.txt", also, &blake3::hash(also).to_hex().to_string());

        let ack = transfer_put_files_batch(
            TransferPutFilesBatchQuery {
                root: root.to_string_lossy().to_string(),
                cycle_id: 7,
            },
            &body,
        )
        .unwrap();
        assert!(ack.ok);
        assert_eq!(ack.failed.len(), 1);
        assert_eq!(ack.failed[0].rel_path, "bad.txt");
        assert_eq!(fs::read(root.join("dir/good.txt")).unwrap(), good);
        assert_eq!(fs::read(root.join("also.txt")).unwrap(), also);
        assert!(!root.join("bad.txt").exists());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn stream_target_resumes_and_truncates_like_the_chunk_endpoint() {
        let temp = temp_dir("stream_target");
        let root = temp.join("dst");
        fs::create_dir_all(&root).unwrap();
        let query = |offset: u64| TransferReceiveFileChunkQuery {
            root: root.to_string_lossy().to_string(),
            rel_path: "video.bin".to_string(),
            cycle_id: 3,
            size: 100,
            offset,
        };

        // Fresh write at offset 0.
        let mut file = transfer_open_file_stream_target(&query(0)).unwrap();
        file.write_all(b"0123456789").unwrap();
        drop(file);
        // Resume at 10 appends; a longer leftover tail would be truncated.
        let mut file = transfer_open_file_stream_target(&query(10)).unwrap();
        file.write_all(b"abc").unwrap();
        drop(file);
        let tmp = tmp_path(&root, 3, "video.bin");
        assert_eq!(fs::read(&tmp).unwrap(), b"0123456789abc");
        // Truncation on a SHORTER resume offset.
        let file = transfer_open_file_stream_target(&query(5)).unwrap();
        drop(file);
        assert_eq!(fs::metadata(&tmp).unwrap().len(), 5);
        // Offset beyond the tmp length must refuse (nothing to resume from).
        assert!(transfer_open_file_stream_target(&query(50)).is_err());
        // Offset beyond the expected size is invalid outright.
        assert!(transfer_open_file_stream_target(&query(101)).is_err());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn copy_pool_tolerates_bounded_per_file_failures() {
        let temp = temp_dir("copy_pool_tolerates");
        let src = temp.join("src");
        let dst = temp.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        for i in 0..5 {
            fs::write(src.join(format!("good{i}.txt")), b"data").unwrap();
        }
        // Accurate size/mtime metadata (ensure_source_stable compares them).
        let snapshot = take_snapshot_with_excludes(&src, SnapshotMode::Source, &[], false).unwrap();
        let mut entries: Vec<SnapshotEntry> = snapshot
            .into_iter()
            .filter(|entry| entry.file_type == "file")
            .collect();
        for i in 0..3 {
            entries.push(SnapshotEntry {
                rel_path: format!("missing{i}.txt"),
                file_type: "file".to_string(),
                size: 4,
                mtime_ns: 0,
                mode: 0o644,
                hash: None,
            });
        }
        let entry_refs: Vec<&SnapshotEntry> = entries.iter().collect();

        let sync = NativeSyncConfig::default();
        let _meter = progress::begin_transfer("dst_pool", &dst, 0);
        let outcome = copy_entries_parallel(&src, &dst, "dst_pool", 1, &entry_refs, &sync).unwrap();
        assert_eq!(outcome.transferred, 5);
        assert_eq!(outcome.failed.len(), 3);
        assert!(outcome.changing.is_empty());
        for i in 0..5 {
            assert_eq!(fs::read(dst.join(format!("good{i}.txt"))).unwrap(), b"data");
        }

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn copy_pool_gives_up_at_the_failure_cap() {
        let temp = temp_dir("copy_pool_cap");
        let src = temp.join("src");
        let dst = temp.join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let entries: Vec<SnapshotEntry> = (0..MAX_PER_FILE_TRANSFER_FAILURES + 5)
            .map(|i| SnapshotEntry {
                rel_path: format!("missing{i}.txt"),
                file_type: "file".to_string(),
                size: 4,
                mtime_ns: 0,
                mode: 0o644,
                hash: None,
            })
            .collect();
        let entry_refs: Vec<&SnapshotEntry> = entries.iter().collect();

        let sync = NativeSyncConfig::default();
        let _meter = progress::begin_transfer("dst_pool", &dst, 0);
        let err = copy_entries_parallel(&src, &dst, "dst_pool", 1, &entry_refs, &sync)
            .expect_err("a doomed destination must abort, not grind through every file");
        assert!(format!("{err:#}").contains("giving up after"), "{err:#}");

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn paused_destination_holds_pending_target_until_resume() {
        let temp = temp_dir("paused_dst_hold");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: true,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();

        // The scheduler assigns a paused destination no target (even a first
        // sync), and manual sync requests are refused outright.
        assert_eq!(
            state.advance_due_destination_targets(&cfg).unwrap().len(),
            0
        );
        let err =
            queue_destination_sync(&cfg, &state, "src_1", "dst_1", SyncRequestMode::Incremental)
                .unwrap_err();
        assert!(err.to_string().contains("paused"), "{err:#}");

        // A target pending from before the pause (the stopped first sync) is
        // held — the engine drives no work and reports the pause.
        state
            .force_target_destination(&cfg, "src_1", "dst_1")
            .unwrap()
            .unwrap();
        sync_all_pending(&cfg, &mut state).unwrap();
        assert!(!dst.join("src").join("hello.txt").exists());
        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views[0].status_reason, "paused");

        // Resuming continues the held target with no new request.
        cfg.source_groups[0].destinations[0].paused = false;
        sync_all_pending(&cfg, &mut state).unwrap();
        assert_eq!(
            fs::read(dst.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );
        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views[0].status, "green");

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_destination_now_only_targets_selected_destination() {
        let temp = temp_dir("sync_one_dst_now");
        let src = temp.join("src");
        let dst_1 = temp.join("dst_1");
        let dst_2 = temp.join("dst_2");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst_1).unwrap();
        fs::create_dir_all(&dst_2).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![
                DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_1.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_2.clone(),
                    enabled: true,
                    schedule: ScheduleConfig {
                        mode: ScheduleMode::Daily,
                        ..ScheduleConfig::default()
                    },
                    paused: false,
                    sync: None,
                },
            ],
        });

        let mut state = State::open(&db).unwrap();
        sync_destination_now(&cfg, &mut state, "src_1", "dst_2").unwrap();

        assert!(!dst_1.join("src").join("hello.txt").exists());
        assert_eq!(
            fs::read(dst_2.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );

        let views = state.destination_views(&cfg).unwrap();
        let first = views
            .iter()
            .find(|view| view.destination_id == "dst_1")
            .unwrap();
        let second = views
            .iter()
            .find(|view| view.destination_id == "dst_2")
            .unwrap();
        assert_eq!(first.target_cycle_id, None);
        assert_eq!(first.last_verified_cycle_id, None);
        assert_eq!(second.status, "green");
        assert_eq!(second.target_cycle_id, second.last_verified_cycle_id);

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn full_destination_sync_only_targets_selected_destination() {
        let temp = temp_dir("full_sync_one_dst_now");
        let src = temp.join("src");
        let dst_1 = temp.join("dst_1");
        let dst_2 = temp.join("dst_2");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst_1).unwrap();
        fs::create_dir_all(&dst_2).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![
                DestinationConfig {
                    id: "dst_1".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_1.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
                DestinationConfig {
                    id: "dst_2".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_2.clone(),
                    enabled: true,
                    schedule: ScheduleConfig {
                        mode: ScheduleMode::Daily,
                        ..ScheduleConfig::default()
                    },
                    paused: false,
                    sync: None,
                },
            ],
        });

        let mut state = State::open(&db).unwrap();
        sync_destination_now_with_mode(&cfg, &mut state, "src_1", "dst_2", SyncRequestMode::Full)
            .unwrap();

        assert!(!dst_1.join("src").join("hello.txt").exists());
        assert_eq!(
            fs::read(dst_2.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );

        let views = state.destination_views(&cfg).unwrap();
        let first = views
            .iter()
            .find(|view| view.destination_id == "dst_1")
            .unwrap();
        let second = views
            .iter()
            .find(|view| view.destination_id == "dst_2")
            .unwrap();
        assert_eq!(first.target_cycle_id, None);
        assert_eq!(first.last_verified_cycle_id, None);
        assert_eq!(second.status, "green");
        assert_eq!(second.target_cycle_id, second.last_verified_cycle_id);

        let cycle_id = second.target_cycle_id.unwrap();
        let needs_full_rescan: i64 = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT needs_full_rescan FROM sync_cycle WHERE id=?1",
                rusqlite::params![cycle_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(needs_full_rescan, 1);

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn directory_source_can_sync_flat_into_destination_root() {
        let temp = temp_dir("flat_directory_destination");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: false,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        assert_eq!(fs::read(dst.join("hello.txt")).unwrap(), b"hello");
        assert!(!dst.join("src").join("hello.txt").exists());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn realtime_destination_manual_full_request_runs_full_sync() {
        let temp = temp_dir("realtime_manual_full_runs_full");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst.join("src")).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();
        fs::write(dst.join("src").join("extra.txt"), b"extra").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_destination_now_with_mode(&cfg, &mut state, "src_1", "dst_1", SyncRequestMode::Full)
            .unwrap();

        let view = state
            .destination_views(&cfg)
            .unwrap()
            .into_iter()
            .find(|view| view.destination_id == "dst_1")
            .unwrap();
        assert_eq!(view.status, "green");
        assert_eq!(
            fs::read(dst.join("src").join("hello.txt")).unwrap(),
            b"hello"
        );
        assert!(!dst.join("src").join("extra.txt").exists());
        let cycle_id = view.target_cycle_id.unwrap();
        let (needs_full_rescan, manual_full_rescan): (i64, i64) = rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT needs_full_rescan, manual_full_rescan FROM sync_cycle WHERE id=?1",
                rusqlite::params![cycle_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(needs_full_rescan, 1);
        assert_eq!(manual_full_rescan, 1);

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn zfs_diff_phase_label_reports_the_cycle_span() {
        assert_eq!(
            zfs_diff_phase_label(
                "ssd@auto_sync_src_4_000000000057",
                "ssd@auto_sync_src_4_000000000085"
            ),
            "zfs diff 57→85"
        );
        // Same base and target (nothing to catch up): plain label.
        assert_eq!(
            zfs_diff_phase_label(
                "ssd@auto_sync_src_4_000000000085",
                "ssd@auto_sync_src_4_000000000085"
            ),
            "zfs diff"
        );
        // Unparseable names fall back gracefully.
        assert_eq!(zfs_diff_phase_label("weird", "names"), "zfs diff");
    }

    #[test]
    fn manual_full_bypasses_standby_asleep_but_not_mount_safety() {
        let asleep = crate::core::standby::Gate::Asleep {
            pool: "zfs".to_string(),
            reason: "disk zfs in standby until Sat 2026-07-18 09:00".to_string(),
        };
        let not_mounted = crate::core::standby::Gate::NotMounted {
            pool: "zfs".to_string(),
            reason: "disk zfs not mounted".to_string(),
        };

        assert!(standby_gate_blocks_sync(&asleep, false));
        assert!(!standby_gate_blocks_sync(&asleep, true));
        assert!(standby_gate_blocks_sync(&not_mounted, false));
        assert!(standby_gate_blocks_sync(&not_mounted, true));
    }

    #[test]
    fn source_changing_failure_advances_verified_and_defers_the_changed_paths() {
        // Plan A: when the source mutates under the copy (a live, non-snapshot
        // source like an app directory being written continuously), the pass
        // must still advance the destination's verified offset so the scheduler
        // stops re-running a full reconcile forever, and it must re-record the
        // changed paths as events so the next cycle picks them up.
        let temp = temp_dir("source_changing_defers");
        let db = temp.join("state.sqlite");
        let state = State::open(&db).unwrap();

        let mut paths = BTreeSet::new();
        paths.insert("docs/live.log".to_string());
        let err = source_changing_error(&paths);

        record_destination_failure(&state, "src_1", "dst_1", 42, &err).unwrap();

        let offset = state.destination_offset("src_1", "dst_1").unwrap();
        assert_eq!(
            offset.last_verified_cycle_id,
            Some(42),
            "source-changing pass must advance verified so the scheduler stops re-driving"
        );
        assert_eq!(offset.status, "yellow");
        assert_eq!(offset.status_reason, "source_changing");

        let open = state.current_open_cycle_id("src_1").unwrap().unwrap();
        assert!(
            state.cycle_has_actionable_events("src_1", open).unwrap(),
            "the changed path must be re-recorded as an event for the next cycle"
        );

        // A real (non-source-changing) failure must still leave the destination
        // red and NOT advance verified.
        let hard = anyhow!("connection refused");
        record_destination_failure(&state, "src_1", "dst_2", 42, &hard).unwrap();
        let hard_offset = state.destination_offset("src_1", "dst_2").unwrap();
        assert_eq!(hard_offset.last_verified_cycle_id, None);
        assert_eq!(hard_offset.status, "red");

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn realtime_event_incremental_syncs_only_event_paths() {
        let temp = temp_dir("realtime_event_paths_only");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();
        fs::write(src.join("untouched.txt"), b"untouched").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();
        let effective_dst = dst.join("src");
        fs::write(effective_dst.join("destination-only.txt"), b"extra").unwrap();
        fs::write(src.join("hello.txt"), b"hello again").unwrap();

        state
            .record_event("src_1", 0, "modify", Some("hello.txt"), false)
            .unwrap();
        assert_eq!(
            state.advance_due_destination_targets(&cfg).unwrap().len(),
            1
        );
        sync_all_pending(&cfg, &mut state).unwrap();

        assert_eq!(
            fs::read(effective_dst.join("hello.txt")).unwrap(),
            b"hello again"
        );
        assert_eq!(
            fs::read(effective_dst.join("untouched.txt")).unwrap(),
            b"untouched"
        );
        assert_eq!(
            fs::read(effective_dst.join("destination-only.txt")).unwrap(),
            b"extra"
        );

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn sync_defers_while_a_compare_runs_for_the_same_destination() {
        let temp = temp_dir("sync_defers_during_compare");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "defer_src".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "defer_dst".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        // Simulate a running compare for this destination on another thread:
        // register its scoped op the way scan_destination_now does. It must
        // live on a separate thread so this thread's sync does not inherit
        // the compare token.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        let compare_thread = std::thread::spawn(move || {
            let _op = cancel::begin_target(
                cancel::KIND_COMPARE,
                Some(cancel::target_for("defer_src", "defer_dst")),
            );
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();

        sync_all_now(&cfg, &mut state).unwrap();
        let effective_dst = dst.join("src");
        assert!(
            !effective_dst.join("hello.txt").exists(),
            "sync must wait for the running compare"
        );
        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views[0].status_reason, "waiting_for_compare");

        // Compare finished: the still-pending target syncs on the next pass.
        release_tx.send(()).unwrap();
        compare_thread.join().unwrap();
        sync_all_pending(&cfg, &mut state).unwrap();
        assert_eq!(fs::read(effective_dst.join("hello.txt")).unwrap(), b"hello");
        let views = state.destination_views(&cfg).unwrap();
        assert_eq!(views[0].status_reason, "verified");

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn repair_scan_syncs_only_reported_differences_and_consumes_the_report() {
        let temp = temp_dir("repair_scan_targets_report_paths");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();
        fs::write(src.join("untouched.txt"), b"untouched").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "scan_repair_src".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "scan_repair_dst".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        // Introduce destination drift the event stream never saw.
        let effective_dst = dst.join("src");
        fs::write(effective_dst.join("hello.txt"), b"corrupted").unwrap();
        fs::remove_file(effective_dst.join("untouched.txt")).unwrap();

        let report =
            scan_destination_now(&cfg, &state, "scan_repair_src", "scan_repair_dst").unwrap();
        assert_eq!(report.to_update, 1, "hello.txt differs");
        assert_eq!(report.to_add, 1, "untouched.txt missing");

        queue_destination_sync(
            &cfg,
            &state,
            "scan_repair_src",
            "scan_repair_dst",
            SyncRequestMode::RepairScan,
        )
        .unwrap();
        // The consumed report is gone: the UI repair affordance clears.
        assert!(
            state
                .get_scan_report("scan_repair_src", "scan_repair_dst")
                .unwrap()
                .is_none()
        );
        sync_all_pending(&cfg, &mut state).unwrap();

        assert_eq!(fs::read(effective_dst.join("hello.txt")).unwrap(), b"hello");
        assert_eq!(
            fs::read(effective_dst.join("untouched.txt")).unwrap(),
            b"untouched"
        );

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn parses_zfs_diff_into_source_relative_paths() {
        let root = Path::new("/tank/photos");
        let output = "M\t/tank/photos/\n\
                      M\t/tank/photos/a.jpg\n\
                      +\t/tank/photos/sub/new.jpg\n\
                      -\t/tank/photos/old.jpg\n\
                      R\t/tank/photos/from.jpg\t/tank/photos/to.jpg\n\
                      M\t/tank/photos/with\\040space.jpg\n\
                      M\t/other/outside.jpg\n";
        let paths = parse_zfs_diff(output, root);
        assert_eq!(
            diff_path_rels(&paths),
            vec![
                "a.jpg".to_string(),
                "from.jpg".to_string(),
                "old.jpg".to_string(),
                "sub/new.jpg".to_string(),
                "to.jpg".to_string(),
                "with space.jpg".to_string(),
            ]
        );
        // M entries only re-examine the entry itself; +/-/R carry subtrees.
        let recursion: Vec<bool> = paths.iter().map(|p| p.recursive).collect();
        assert_eq!(recursion, vec![false, true, true, true, true, false]);
        // The dataset root itself and paths outside the source root are skipped.
        assert!(!paths.iter().any(|p| p.rel.contains("outside")));
    }

    #[test]
    fn zfs_diff_m_dir_listed_under_multiple_kinds_stays_recursive() {
        let root = Path::new("/tank");
        let output = "M\t/tank/dir\n+\t/tank/dir\n";
        let paths = parse_zfs_diff(output, root);
        assert_eq!(
            paths,
            vec![DiffPath {
                rel: "dir".to_string(),
                recursive: true,
            }]
        );
    }

    #[test]
    fn diff_path_snapshot_skips_descendants_of_recursive_ancestors() {
        let temp =
            std::env::temp_dir().join(format!("auto_sync_diff_dedup_{}", std::process::id()));
        fs::create_dir_all(temp.join("new_dir/sub")).unwrap();
        fs::write(temp.join("new_dir/a.txt"), b"a").unwrap();
        fs::write(temp.join("new_dir/sub/b.txt"), b"b").unwrap();
        fs::write(temp.join("touched.txt"), b"t").unwrap();
        // Mirrors zfs diff output for a created dir: the dir (recursive) plus
        // every child listed individually — the walk must not repeat per child.
        let paths = vec![
            DiffPath {
                rel: "new_dir".to_string(),
                recursive: true,
            },
            DiffPath {
                rel: "new_dir/a.txt".to_string(),
                recursive: true,
            },
            DiffPath {
                rel: "new_dir/sub".to_string(),
                recursive: true,
            },
            DiffPath {
                rel: "new_dir/sub/b.txt".to_string(),
                recursive: true,
            },
            DiffPath {
                rel: "touched.txt".to_string(),
                recursive: false,
            },
        ];
        let entries = take_snapshot_diff_paths_with_excludes(
            &temp,
            &paths,
            SnapshotMode::Destination,
            &[],
            false,
        )
        .unwrap();
        let rels: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "new_dir",
                "new_dir/a.txt",
                "new_dir/sub",
                "new_dir/sub/b.txt",
                "touched.txt"
            ]
        );
        fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn diff_path_snapshot_m_dir_takes_only_the_dir_entry() {
        let temp = std::env::temp_dir().join(format!("auto_sync_diff_mdir_{}", std::process::id()));
        fs::create_dir_all(temp.join("big_dir")).unwrap();
        fs::write(temp.join("big_dir/child.txt"), b"c").unwrap();
        let paths = vec![DiffPath {
            rel: "big_dir".to_string(),
            recursive: false,
        }];
        let entries = take_snapshot_diff_paths_with_excludes(
            &temp,
            &paths,
            SnapshotMode::Destination,
            &[],
            false,
        )
        .unwrap();
        let rels: Vec<&str> = entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert_eq!(rels, vec!["big_dir"], "children have their own diff lines");
        fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn snapshot_stream_roundtrips_entries_and_matches_buffered_walk() {
        let temp =
            std::env::temp_dir().join(format!("auto_sync_snap_stream_{}", std::process::id()));
        fs::create_dir_all(temp.join("dir")).unwrap();
        fs::write(temp.join("dir/a.txt"), b"aaa").unwrap();
        fs::write(temp.join("top.txt"), b"t").unwrap();

        let req = TransferSnapshotRequest {
            root: temp.clone(),
            mode: TransferSnapshotMode::Destination,
            excludes: Vec::new(),
            checksum: true,
            purpose: String::new(),
            scope: String::new(),
        };
        let mut wire = Vec::new();
        transfer_snapshot_stream(req.clone(), &mut |buf| {
            wire.extend_from_slice(&buf);
            Ok(())
        })
        .unwrap();
        let streamed: Vec<SnapshotEntry> = wire
            .split(|&b| b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        let buffered = transfer_snapshot(req).unwrap();
        assert_eq!(
            serde_json::to_value(&streamed).unwrap(),
            serde_json::to_value(&buffered).unwrap()
        );
        assert!(streamed.iter().any(|e| e.rel_path == "dir/a.txt"));
        fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn nested_mounts_detection_flags_foreign_mounts_only() {
        let mounts = "\
zfs_pool/data /zfs zfs rw 0 0
zfs_pool/nested /zfs/nested zfs rw 0 0
tmpfs /zfs/scratch tmpfs rw 0 0
zfs_pool/data@snap /zfs/.zfs/snapshot/snap zfs ro 0 0
other /zfs_other zfs rw 0 0
";
        let nested = nested_mounts_in(mounts, Path::new("/zfs"));
        assert_eq!(
            nested,
            vec![PathBuf::from("/zfs/nested"), PathBuf::from("/zfs/scratch")]
        );
        assert!(nested_mounts_in(mounts, Path::new("/zfs_other")).is_empty());
    }

    #[test]
    fn zfs_diff_union_excludes_internal_trash_tmp_probe() {
        // A mirror delete renames into .auto_sync_trash, so the destination's
        // base->live diff always names the trash afterwards. Those paths must
        // never enter the union: recursing them turned the whole recycle bin
        // (~80K entries live) into per-path snapshot entries all reported as
        // false "delete" differences.
        let root = Path::new("/zfs_pool");
        let output = "M\t/zfs_pool/\n\
                      R\t/zfs_pool/gone.txt\t/zfs_pool/.auto_sync_trash/29/gone.txt\n\
                      +\t/zfs_pool/.auto_sync_trash/29\n\
                      M\t/zfs_pool/.auto_sync_trash\n\
                      M\t/zfs_pool/.auto_sync_tmp/12/half.bin\n\
                      M\t/zfs_pool/xwechat_files/.auto_sync_tmp/2363/half.bin\n\
                      +\t/zfs_pool/.auto_sync_probe\n\
                      M\t/zfs_pool/real_change.txt\n";
        let paths = parse_zfs_diff(output, root);
        assert_eq!(
            diff_path_rels(&paths),
            vec!["gone.txt".to_string(), "real_change.txt".to_string()]
        );

        // Defense in depth: even if an internal path reaches the per-path
        // snapshot layer, it is skipped instead of recursed.
        let temp =
            std::env::temp_dir().join(format!("auto_sync_internal_snap_{}", std::process::id()));
        fs::create_dir_all(temp.join(INTERNAL_TRASH).join("29")).unwrap();
        fs::write(temp.join(INTERNAL_TRASH).join("29").join("f.txt"), b"x").unwrap();
        let entries = take_snapshot_diff_paths_with_excludes(
            &temp,
            &diff_paths_all_recursive(&[
                format!("{INTERNAL_TRASH}/29"),
                INTERNAL_TRASH.to_string(),
            ]),
            SnapshotMode::Destination,
            &[],
            false,
        )
        .unwrap();
        assert!(entries.is_empty());
        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn internal_path_detection_matches_nested_segments() {
        assert!(rel_str_is_internal(".auto_sync_tmp/1/file"));
        assert!(rel_str_is_internal("xwechat_files/.auto_sync_tmp/1/file"));
        assert!(rel_str_is_internal("xwechat_files\\.auto_sync_trash\\old"));
        assert!(!rel_str_is_internal("xwechat_files/real/file"));
    }

    #[test]
    fn evidence_paths_use_exact_mtime_comparison() {
        // A same-size rewrite landing within the modify window of the
        // previously synced version passes the whole-tree quick check but must
        // NOT pass on an evidence path (watcher event / zfs diff): that was a
        // permanent silent-drift hole.
        let sync = NativeSyncConfig::default(); // modify_window_secs = 1
        let mut old = test_file_entry("f.txt", 10);
        let mut new = test_file_entry("f.txt", 10);
        old.mtime_ns = 1_000_000_000_000;
        new.mtime_ns = old.mtime_ns + 800_000_000; // +0.8s, same size
        assert!(
            entries_match(&new, &old, &sync),
            "walk quick check tolerates"
        );
        assert!(
            !entries_match_exact(&new, &old, &sync),
            "evidence path must treat it as changed"
        );
        // Sub-microsecond skew (NTFS FILETIME truncation) still matches.
        new.mtime_ns = old.mtime_ns + 100;
        assert!(entries_match_exact(&new, &old, &sync));

        // Hash evidence wins when both sides carry one, even without checksum
        // mode: an mtime-restoring rewrite cannot clear itself.
        let mut tampered = test_file_entry("f.txt", 10);
        tampered.mtime_ns = old.mtime_ns;
        tampered.hash = Some("different".to_string());
        old.hash = Some("original".to_string());
        assert!(!entries_match(&tampered, &old, &sync));

        // Mode-only drift: content matches, permissions differ.
        let mut chmod = test_file_entry("f.txt", 10);
        chmod.mtime_ns = 1_000_000_000_000;
        chmod.mode = 0o600;
        let mut base = test_file_entry("f.txt", 10);
        base.mtime_ns = 1_000_000_000_000;
        base.mode = 0o644;
        assert!(entries_match_exact(&chmod, &base, &sync));
        assert!(entry_mode_differs(&chmod, &base));
    }

    #[test]
    fn unescapes_zfs_octal_paths() {
        assert_eq!(unescape_zfs_path("a\\040b"), "a b");
        assert_eq!(unescape_zfs_path("tab\\011x"), "tab\tx");
        assert_eq!(unescape_zfs_path("plain"), "plain");
        // A backslash not followed by three octal digits is left as-is.
        assert_eq!(unescape_zfs_path("back\\slash"), "back\\slash");
    }

    #[test]
    fn realtime_rescan_event_triggers_full_reconcile() {
        // A possible-event-loss signal (here a rescan_required event, as recorded
        // on queue overflow / USN gap) must trigger a full reconcile that repairs
        // every difference — including a destination-only file the event stream
        // never mentioned — instead of the event-path incremental sync.
        let temp = temp_dir("realtime_rescan_full_reconcile");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("hello.txt"), b"hello").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: Vec::new(),
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();
        let effective_dst = dst.join("src");

        // A change the watcher missed (no event), plus a destination-only extra.
        fs::write(effective_dst.join("destination-only.txt"), b"extra").unwrap();
        fs::write(src.join("late.txt"), b"late").unwrap();

        // Only a possible-loss marker is recorded — no path-level events.
        state
            .record_event("src_1", 0, "queue_overflow", None, true)
            .unwrap();
        assert_eq!(
            state.advance_due_destination_targets(&cfg).unwrap().len(),
            1
        );
        sync_all_pending(&cfg, &mut state).unwrap();

        // Full reconcile: the unmentioned new file is copied and the
        // destination-only extra is mirror-deleted.
        assert_eq!(fs::read(effective_dst.join("late.txt")).unwrap(), b"late");
        assert!(!effective_dst.join("destination-only.txt").exists());

        let views = state.destination_views(&cfg).unwrap();
        let view = views.iter().find(|v| v.destination_id == "dst_1").unwrap();
        assert_eq!(view.status, "green");
        assert_eq!(view.target_cycle_id, view.last_verified_cycle_id);

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn destination_sync_config_overrides_global_mirror() {
        let temp = temp_dir("dst_sync_override");
        let src = temp.join("src");
        let dst_a = temp.join("dst_a");
        let dst_b = temp.join("dst_b");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(dst_a.join("src")).unwrap();
        fs::create_dir_all(dst_b.join("src")).unwrap();
        fs::write(src.join("keep.txt"), b"keep").unwrap();
        fs::write(dst_a.join("src").join("extra.txt"), b"extra").unwrap();
        fs::write(dst_b.join("src").join("extra.txt"), b"extra").unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.app.sync.mirror = true;
        let mut dst_b_sync = cfg.app.sync.clone();
        dst_b_sync.mirror = false;
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: vec![],
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig::default(),
            destinations: vec![
                DestinationConfig {
                    id: "dst_a".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_a.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: None,
                },
                DestinationConfig {
                    id: "dst_b".to_string(),
                    machine_id: "local".to_string(),
                    path: dst_b.clone(),
                    enabled: true,
                    schedule: ScheduleConfig::default(),
                    paused: false,
                    sync: Some(dst_b_sync),
                },
            ],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        assert!(!dst_a.join("src").join("extra.txt").exists());
        assert!(dst_b.join("src").join("extra.txt").exists());
        assert_eq!(
            fs::read(dst_a.join("src").join("keep.txt")).unwrap(),
            b"keep"
        );
        assert_eq!(
            fs::read(dst_b.join("src").join("keep.txt")).unwrap(),
            b"keep"
        );

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn excludes_source_paths_without_deleting_existing_destination_paths() {
        let temp = temp_dir("exclude_paths");
        let src = temp.join("src");
        let dst = temp.join("dst");
        let db = temp.join("state.sqlite");
        fs::create_dir_all(src.join("skip_dir")).unwrap();
        // effective dst root is dst/src because the source dir is named "src"
        fs::create_dir_all(dst.join("src").join("skip_dir")).unwrap();
        fs::write(src.join("keep.txt"), b"keep").unwrap();
        fs::write(src.join("skip.txt"), b"skip source").unwrap();
        fs::write(src.join("skip_dir").join("nested.txt"), b"skip nested").unwrap();
        fs::write(dst.join("src").join("skip.txt"), b"existing skip").unwrap();
        fs::write(
            dst.join("src").join("skip_dir").join("nested.txt"),
            b"existing nested",
        )
        .unwrap();

        let mut cfg = AppConfig::default();
        cfg.app.data_db = db.clone();
        cfg.source_groups.push(SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: src.clone(),
            add_directory: true,
            managed_by: String::new(),
            excludes: vec![PathBuf::from("skip.txt"), PathBuf::from("skip_dir")],
            enabled: true,
            order: 0,
            mode: SyncMode::Mirror,
            snapshot: SnapshotConfig {
                backend: SnapshotBackend::Manifest,
                ..SnapshotConfig::default()
            },
            destinations: vec![DestinationConfig {
                id: "dst_1".to_string(),
                machine_id: "local".to_string(),
                path: dst.clone(),
                enabled: true,
                schedule: ScheduleConfig::default(),
                paused: false,
                sync: None,
            }],
        });

        let mut state = State::open(&db).unwrap();
        sync_all_now(&cfg, &mut state).unwrap();

        let eff = dst.join("src");
        assert_eq!(fs::read(eff.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(eff.join("skip.txt")).unwrap(), b"existing skip");
        assert_eq!(
            fs::read(eff.join("skip_dir").join("nested.txt")).unwrap(),
            b"existing nested"
        );
        assert!(!eff.join(".auto_sync_trash").exists());

        fs::remove_dir_all(temp).ok();
    }

    #[test]
    fn extracts_source_changed_paths_from_error_chain() {
        let err = anyhow!(
            "source changed while copying log/live.log\nsource changed while copying log/other.log"
        )
        .context("failed to copy log/live.log");

        assert_eq!(
            source_changed_paths(&err),
            vec!["log/live.log".to_string(), "log/other.log".to_string()]
        );
    }

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("auto_sync_{name}_{}_{}", std::process::id(), nanos));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn test_file_entry(rel_path: &str, size: i64) -> SnapshotEntry {
        SnapshotEntry {
            rel_path: rel_path.to_string(),
            file_type: "file".to_string(),
            size,
            mtime_ns: 123,
            mode: 0o644,
            hash: None,
        }
    }

    #[test]
    fn scan_report_classifies_differences() {
        let source = vec![
            test_file_entry("same.txt", 10),
            test_file_entry("changed.txt", 20),
            test_file_entry("new.txt", 5),
        ];
        let dst = vec![
            test_file_entry("same.txt", 10),
            test_file_entry("changed.txt", 21),
            test_file_entry("extra.txt", 7),
        ];
        let mut sync = NativeSyncConfig::default();
        sync.mirror = true;
        sync.checksum = false;
        let report = build_scan_report("s", "d", &source, &dst, &[], &sync, false);
        assert_eq!(report.to_add, 1, "new.txt");
        assert_eq!(report.to_update, 1, "changed.txt");
        assert_eq!(report.to_delete, 1, "extra.txt");
        assert_eq!(report.in_sync, 1, "same.txt");
        assert_eq!(report.differences.len(), 3);
        assert!(!report.truncated);

        // Mirror off: extra destination files are not flagged for deletion.
        sync.mirror = false;
        let report = build_scan_report("s", "d", &source, &dst, &[], &sync, false);
        assert_eq!(report.to_delete, 0);
    }

    #[test]
    fn scheduled_destination_applies_event_backlog_across_cycles() -> Result<()> {
        // A scheduled (non-realtime) destination accumulates watcher events
        // across every cycle since its last verified one and applies the whole
        // backlog when its schedule comes due — not just the target cycle's
        // events, and no full re-scan.
        let temp = temp_dir("scheduled_event_backlog");
        let db = temp.join("state.sqlite");
        let source = SourceGroupConfig {
            id: "src_1".to_string(),
            machine_id: "local".to_string(),
            src: temp.join("src"),
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
                path: temp.join("dst"),
                enabled: true,
                schedule: ScheduleConfig {
                    mode: crate::core::config::ScheduleMode::Daily,
                    ..ScheduleConfig::default()
                },
                paused: false,
                sync: None,
            }],
        };
        let state = State::open(&db)?;

        // Baseline: destination verified at cycle A.
        let cycle_a = state.ensure_open_cycle("src_1", Utc::now())?;
        state.close_current_cycle_for_source("src_1")?;
        state.upsert_destination_status("src_1", "dst_1", Some(cycle_a), "green", "verified")?;

        // Two later cycles each accumulate events the destination never saw.
        state.record_event("src_1", 0, "modify", Some("first.txt"), false)?;
        state.close_current_cycle_for_source("src_1")?;
        state.record_event("src_1", 0, "modify", Some("second.txt"), false)?;
        let target = state
            .close_current_cycle_for_source("src_1")?
            .expect("target cycle");
        state.set_destination_target("src_1", "dst_1", target.id)?;

        let plan = event_incremental_plan(&state, &source, &target, &[0])?
            .expect("scheduled destinations are event-plan eligible");
        match plan {
            RealtimeIncrementalPlan::Apply(per_dst) => {
                assert_eq!(per_dst.len(), 1);
                let (dst_index, paths) = &per_dst[0];
                assert_eq!(*dst_index, 0);
                assert_eq!(
                    paths,
                    &vec!["first.txt".to_string(), "second.txt".to_string()],
                    "backlog spans every cycle since last verified"
                );
            }
            RealtimeIncrementalPlan::Unusable(reason) => panic!("unusable: {reason}"),
        }

        // A rescan-required event anywhere in the backlog forces a full pass.
        state.record_event("src_1", 0, "queue_overflow", None, true)?;
        let target = state
            .close_current_cycle_for_source("src_1")?
            .expect("overflow cycle");
        state.set_destination_target("src_1", "dst_1", target.id)?;
        assert!(
            event_incremental_plan(&state, &source, &target, &[0])?.is_none(),
            "possible event loss must fall back to full reconcile"
        );

        fs::remove_dir_all(&temp).ok();
        Ok(())
    }

    #[test]
    fn ensure_source_stable_detects_same_size_mutation() -> Result<()> {
        let temp = temp_dir("source_stable");
        let path = temp.join("live.txt");
        fs::write(&path, b"12345")?;
        let entry = snapshot_entry(&path, "live.txt".to_string(), false)?;
        ensure_source_stable(&path, &entry)?;

        // Same size, different content: only the mtime betrays the change.
        fs::write(&path, b"abcde")?;
        filetime::set_file_mtime(
            &path,
            FileTime::from_unix_time(entry.mtime_ns / 1_000_000_000 + 5, 0),
        )?;
        let err = ensure_source_stable(&path, &entry).unwrap_err();
        assert_eq!(source_changed_paths(&err), vec!["live.txt".to_string()]);
        fs::remove_dir_all(&temp).ok();
        Ok(())
    }

    #[test]
    fn diff_manifests_classifies_all_kinds() {
        let dir_entry = |rel: &str| SnapshotEntry {
            rel_path: rel.to_string(),
            file_type: "dir".to_string(),
            size: 0,
            mtime_ns: 1,
            mode: 0o755,
            hash: None,
        };
        let source = vec![
            test_file_entry("same.txt", 10),
            test_file_entry("changed.txt", 20),
            test_file_entry("new.txt", 5),
            dir_entry("new_dir"),
            dir_entry("same_dir"),
            test_file_entry("was_dir_now_file", 3),
            test_file_entry("excluded/skip.txt", 1),
        ];
        let dst = vec![
            test_file_entry("same.txt", 10),
            test_file_entry("changed.txt", 21),
            dir_entry("same_dir"),
            dir_entry("was_dir_now_file"),
            test_file_entry("extra.txt", 7),
        ];
        let mut sync = NativeSyncConfig::default();
        sync.mirror = true;
        sync.checksum = false;
        let excludes = vec![PathBuf::from("excluded")];
        let diff = diff_manifests(&source, &dst, &excludes, &sync, false);

        assert_eq!(
            diff.transfer
                .iter()
                .map(|(e, existing)| (e.rel_path.as_str(), existing.is_some()))
                .collect::<Vec<_>>(),
            vec![("changed.txt", true), ("new.txt", false)]
        );
        assert_eq!(diff.missing_dirs.len(), 1);
        assert_eq!(diff.missing_dirs[0].rel_path, "new_dir");
        assert_eq!(diff.type_mismatch.len(), 1);
        assert_eq!(diff.type_mismatch[0].rel_path, "was_dir_now_file");
        assert_eq!(
            diff.extras
                .iter()
                .map(|e| e.rel_path.as_str())
                .collect::<Vec<_>>(),
            vec!["extra.txt"]
        );
        assert_eq!(diff.in_sync, 2, "same.txt + same_dir");

        // The sync work list re-copies type-flipped files with no delta basis.
        let to_copy = diff.entries_to_copy();
        assert_eq!(
            to_copy
                .iter()
                .map(|(e, basis)| (e.rel_path.as_str(), basis.is_some()))
                .collect::<Vec<_>>(),
            vec![
                ("changed.txt", true),
                ("new.txt", false),
                ("was_dir_now_file", false)
            ]
        );
    }

    #[test]
    fn scan_report_error_field_defaults_for_legacy_json() {
        // Reports persisted by older builds have no `error` field; they must
        // keep deserializing (and an empty error means success).
        let legacy = r#"{
            "source_id":"s","destination_id":"d","scanned_at":"t",
            "source_entries":1,"dst_entries":1,"in_sync":1,"to_add":0,
            "to_update":0,"to_delete":0,"type_mismatch":0,
            "differences":[],"truncated":false
        }"#;
        let report: ScanReport = serde_json::from_str(legacy).unwrap();
        assert!(report.error.is_empty());
    }

    #[test]
    fn snapshot_entry_omits_missing_hash_and_still_round_trips() {
        let entry = test_file_entry("a.txt", 5);
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("hash"), "null hash must be omitted: {json}");
        let back: SnapshotEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entry);
        // Legacy peers still send an explicit null.
        let legacy: SnapshotEntry = serde_json::from_str(
            r#"{"rel_path":"a.txt","file_type":"file","size":5,"mtime_ns":123,"mode":420,"hash":null}"#,
        )
        .unwrap();
        assert_eq!(legacy.hash, None);
    }

    #[test]
    fn snapshot_timeout_floors_the_per_file_transfer_timeout() {
        let mut sync = NativeSyncConfig::default();
        sync.transfer_timeout_secs = 120;
        sync.checksum = false;
        assert_eq!(snapshot_timeout(&sync), Duration::from_secs(3600));
        sync.checksum = true;
        assert_eq!(snapshot_timeout(&sync), Duration::from_secs(6 * 3600));
        sync.transfer_timeout_secs = 24 * 3600;
        assert_eq!(snapshot_timeout(&sync), Duration::from_secs(24 * 3600));
    }

    #[test]
    fn detects_concurrent_scan_rejection() {
        let err = anyhow!("{SCAN_ALREADY_RUNNING}").context("scan wrapper");
        assert!(scan_error_is_already_running(&err));
        assert!(!scan_error_is_already_running(&anyhow!("disk on fire")));
    }

    #[test]
    fn compare_context_tags_scan_progress() {
        // Hold both engine gates so no parallel test's tree walk replaces the
        // global scan-progress slot mid-assertion.
        let _sync_held = sync_gate().lock().unwrap_or_else(|e| e.into_inner());
        let _scan_held = scan_gate().lock().unwrap_or_else(|e| e.into_inner());
        {
            let _compare = progress::enter_compare_context();
            let guard = progress::start_scan(Path::new("compare_root"));
            let view = progress::current_scan_progress().unwrap();
            assert_eq!(view.kind, "compare");
            drop(guard);
        }
        let guard = progress::start_scan(Path::new("sync_root"));
        let view = progress::current_scan_progress().unwrap();
        assert_eq!(view.kind, "sync");
        drop(guard);
    }

    #[test]
    fn source_changed_paths_survives_peer_http_wrapper() {
        // The push-file hop wraps the peer's error text into the non-200
        // message body; the canonical prefix must still parse out of it.
        let err = anyhow!(
            "peer returned non-200 response: HTTP/1.1 500: source changed while copying a/b c.txt"
        );
        assert_eq!(source_changed_paths(&err), vec!["a/b c.txt".to_string()]);
        assert!(transfer_error_is_source_changing(&err));
    }

    #[test]
    fn transfer_error_fatal_classification() {
        let refused = anyhow::Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))
        .context("failed to transfer a.txt");
        assert!(transfer_error_is_fatal(&refused));

        let closed = anyhow!("peer closed connection before HTTP headers");
        assert!(transfer_error_is_fatal(&closed));

        let per_file = anyhow!("peer returned non-200 response: HTTP/1.1 500: permission denied");
        assert!(!transfer_error_is_fatal(&per_file));

        let not_found =
            anyhow::Error::from(io::Error::new(io::ErrorKind::NotFound, "no such file"))
                .context("failed to read source");
        assert!(!transfer_error_is_fatal(&not_found));
    }

    #[test]
    fn transfer_outcome_result_precedence() {
        // Per-file failures dominate (red)...
        let outcome = TransferOutcome {
            transferred: 3,
            changing: BTreeSet::from(["a.txt".to_string()]),
            failed: vec![("b.txt".to_string(), anyhow!("permission denied"))],
        };
        let ignored = outcome.unverifiable_paths();
        assert!(ignored.contains("a.txt") && ignored.contains("b.txt"));
        let err = outcome.into_result().unwrap_err();
        assert!(
            source_changed_paths(&err).is_empty(),
            "failed beats changing: {err:#}"
        );

        // ...tolerated source changes alone stay classifiable (yellow).
        let outcome = TransferOutcome {
            transferred: 3,
            changing: BTreeSet::from(["a.txt".to_string()]),
            failed: Vec::new(),
        };
        let err = outcome.into_result().unwrap_err();
        assert_eq!(source_changed_paths(&err), vec!["a.txt".to_string()]);

        let outcome = TransferOutcome {
            transferred: 3,
            changing: BTreeSet::new(),
            failed: Vec::new(),
        };
        assert!(outcome.into_result().is_ok());
    }

    #[test]
    fn verify_copied_entries_checks_only_given_paths() -> Result<()> {
        let temp = temp_dir("verify_copied_entries");
        let dst = temp.join("dst");
        fs::create_dir_all(&dst)?;
        fs::write(dst.join("ok.txt"), b"hello")?;
        let sync = NativeSyncConfig::default();

        let ok = snapshot_entry(&dst.join("ok.txt"), "ok.txt".to_string(), false)?;
        verify_copied_entries(&dst, [&ok], &BTreeSet::new(), &sync)?;

        // A missing file fails...
        let missing = test_file_entry("missing.txt", 5);
        assert!(verify_copied_entries(&dst, [&missing], &BTreeSet::new(), &sync).is_err());
        // ...unless it is in the ignored (changed/failed mid-copy) set.
        let ignored = BTreeSet::from(["missing.txt".to_string()]);
        verify_copied_entries(&dst, [&missing], &ignored, &sync)?;

        // A size mismatch fails.
        let mut wrong = ok.clone();
        wrong.size += 1;
        assert!(verify_copied_entries(&dst, [&wrong], &BTreeSet::new(), &sync).is_err());
        fs::remove_dir_all(&temp).ok();
        Ok(())
    }

    #[test]
    fn transfer_memory_permits_do_not_deadlock_oversized_requests() {
        // A request larger than the whole budget must proceed once the budget
        // is otherwise idle, and release its reservation on drop.
        let permit = acquire_transfer_memory(TRANSFER_MEMORY_BUDGET * 4);
        drop(permit);
        let small_a = acquire_transfer_memory(1024);
        let small_b = acquire_transfer_memory(1024);
        drop(small_a);
        drop(small_b);
        let (used, _) = transfer_memory();
        assert_eq!(*used.lock().unwrap(), 0);
    }
}
