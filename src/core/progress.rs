use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const STALE_AFTER: Duration = Duration::from_secs(8);
const SPEED_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const EWMA_NEW_SAMPLE_PERCENT: u64 = 35;

static TRANSFER_PROGRESS: OnceLock<Mutex<Option<TransferProgressState>>> = OnceLock::new();
static SCAN_PROGRESS: OnceLock<Mutex<Option<ScanProgressState>>> = OnceLock::new();
static PROGRESS_FILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static SCAN_PROGRESS_FILE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferProgressView {
    pub destination_id: String,
    pub destination_path: String,
    pub rel_path: String,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub bytes_per_sec: u64,
    pub updated_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgressView {
    pub root_path: String,
    pub current_path: String,
    pub entries_seen: u64,
    pub updated_at_ms: u128,
    /// "sync" for backup tree walks, "compare" for dry-run Scan walks; lets the
    /// UI show each activity only in its own view instead of the two stealing
    /// one shared progress display.
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug)]
struct TransferProgressState {
    token: u64,
    destination_id: String,
    destination_path: String,
    rel_path: String,
    transferred_bytes: u64,
    total_bytes: u64,
    bytes_per_sec: u64,
    last_bytes: u64,
    started_at: Instant,
    last_sample_at: Instant,
    updated_at: Instant,
    updated_at_ms: u128,
}

#[derive(Debug)]
struct ScanProgressState {
    token: u64,
    root_path: String,
    current_path: String,
    entries_seen: u64,
    updated_at: Instant,
    last_write_at: Instant,
    updated_at_ms: u128,
    kind: String,
}

thread_local! {
    /// Marks the current thread as running a dry-run compare; tree walks it
    /// starts are tagged "compare" instead of "sync" in the progress view.
    static IN_COMPARE_CONTEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII marker for compare (dry-run Scan) work on this thread. Threads spawned
/// while it is held do NOT inherit it — spawn-site code must re-enter.
pub struct CompareContextGuard {
    previous: bool,
}

impl Drop for CompareContextGuard {
    fn drop(&mut self) {
        IN_COMPARE_CONTEXT.with(|flag| flag.set(self.previous));
    }
}

pub fn enter_compare_context() -> CompareContextGuard {
    let previous = IN_COMPARE_CONTEXT.with(|flag| flag.replace(true));
    CompareContextGuard { previous }
}

pub fn in_compare_context() -> bool {
    IN_COMPARE_CONTEXT.with(|flag| flag.get())
}

pub struct TransferProgressGuard {
    token: u64,
}

pub struct ScanProgressGuard {
    token: u64,
}

impl TransferProgressGuard {
    pub fn update(&self, transferred_bytes: u64) {
        let mut progress = progress_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let Some(state) = progress.as_mut() else {
            return;
        };
        if state.token != self.token {
            return;
        }
        state.transferred_bytes = transferred_bytes.min(state.total_bytes);
        let now = Instant::now();
        state.sample_speed(now, transferred_bytes >= state.total_bytes);
        state.updated_at = now;
        state.updated_at_ms = now_ms();
        write_progress_file(&state.view());
    }
}

impl ScanProgressGuard {
    pub fn update(&self, current_path: &Path, entries_seen: u64) {
        let mut progress = scan_progress_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let Some(state) = progress.as_mut() else {
            return;
        };
        if state.token != self.token {
            return;
        }
        state.current_path = current_path.to_string_lossy().to_string();
        state.entries_seen = entries_seen;
        let now = Instant::now();
        state.updated_at = now;
        state.updated_at_ms = now_ms();
        if now.duration_since(state.last_write_at) >= Duration::from_millis(250) {
            state.last_write_at = now;
            write_scan_progress_file(&state.view());
        }
    }
}

impl Drop for TransferProgressGuard {
    fn drop(&mut self) {
        let mut progress = progress_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if progress
            .as_ref()
            .is_some_and(|state| state.token == self.token)
        {
            *progress = None;
            clear_progress_file();
        }
    }
}

impl Drop for ScanProgressGuard {
    fn drop(&mut self) {
        let mut progress = scan_progress_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if progress
            .as_ref()
            .is_some_and(|state| state.token == self.token)
        {
            *progress = None;
            clear_scan_progress_file();
        }
    }
}

pub fn configure_progress_file(data_db: &Path) {
    let state_dir = data_db.parent().unwrap_or_else(|| Path::new("."));
    let path = state_dir.join("runtime_progress.json");
    let scan_path = state_dir.join("runtime_scan.json");
    fs::create_dir_all(state_dir).ok();
    let mut configured = progress_file_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *configured = Some(path);
    let mut scan_configured = scan_progress_file_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *scan_configured = Some(scan_path);
}

pub fn start_transfer(
    destination_id: &str,
    destination_path: &Path,
    rel_path: &str,
    total_bytes: u64,
    transferred_bytes: u64,
) -> TransferProgressGuard {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut progress = progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *progress = Some(TransferProgressState {
        token,
        destination_id: destination_id.to_string(),
        destination_path: destination_path.to_string_lossy().to_string(),
        rel_path: rel_path.to_string(),
        transferred_bytes: transferred_bytes.min(total_bytes),
        total_bytes,
        bytes_per_sec: 0,
        last_bytes: transferred_bytes.min(total_bytes),
        started_at: now,
        last_sample_at: now,
        updated_at: now,
        updated_at_ms: now_ms(),
    });
    if let Some(state) = progress.as_ref() {
        write_progress_file(&state.view());
    }
    TransferProgressGuard { token }
}

pub fn start_scan(root_path: &Path) -> ScanProgressGuard {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut progress = scan_progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let root_path = root_path.to_string_lossy().to_string();
    *progress = Some(ScanProgressState {
        token,
        root_path: root_path.clone(),
        current_path: root_path,
        entries_seen: 0,
        updated_at: now,
        last_write_at: now,
        updated_at_ms: now_ms(),
        kind: if in_compare_context() { "compare" } else { "sync" }.to_string(),
    });
    if let Some(state) = progress.as_ref() {
        write_scan_progress_file(&state.view());
    }
    ScanProgressGuard { token }
}

/// Begin an aggregate transfer that accumulates bytes across many concurrent
/// files (e.g. a parallel worker pool). Workers report via [`record_transfer`].
pub fn begin_transfer(
    destination_id: &str,
    destination_path: &Path,
    total_bytes: u64,
) -> TransferProgressGuard {
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut progress = progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    *progress = Some(TransferProgressState {
        token,
        destination_id: destination_id.to_string(),
        destination_path: destination_path.to_string_lossy().to_string(),
        rel_path: String::new(),
        transferred_bytes: 0,
        total_bytes,
        bytes_per_sec: 0,
        last_bytes: 0,
        started_at: now,
        last_sample_at: now,
        updated_at: now,
        updated_at_ms: now_ms(),
    });
    if let Some(state) = progress.as_ref() {
        write_progress_file(&state.view());
    }
    TransferProgressGuard { token }
}

/// Add `added` bytes to the active aggregate transfer and note the current file.
/// Safe to call from many threads; a no-op if no aggregate transfer is active.
pub fn record_transfer(rel_path: &str, added: u64) {
    let mut progress = progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let Some(state) = progress.as_mut() else {
        return;
    };
    state.transferred_bytes = state.transferred_bytes.saturating_add(added);
    if state.total_bytes > 0 {
        state.transferred_bytes = state.transferred_bytes.min(state.total_bytes);
    }
    if !rel_path.is_empty() {
        state.rel_path = rel_path.to_string();
    }
    let now = Instant::now();
    state.updated_at = now;
    state.updated_at_ms = now_ms();
    if state.sample_speed(now, false) {
        write_progress_file(&state.view());
    }
}

pub fn current_transfer_progress() -> Option<TransferProgressView> {
    let mut progress = progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if let Some(state) = progress.as_mut() {
        if state.updated_at.elapsed() > STALE_AFTER {
            *progress = None;
            clear_progress_file();
        } else {
            let now = Instant::now();
            if state.sample_speed(now, false) {
                state.updated_at = now;
                state.updated_at_ms = now_ms();
                write_progress_file(&state.view());
            }
            return Some(state.view());
        }
    }
    read_progress_file()
}

pub fn current_scan_progress() -> Option<ScanProgressView> {
    let mut progress = scan_progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if let Some(state) = progress.as_ref() {
        if state.updated_at.elapsed() > STALE_AFTER {
            *progress = None;
            clear_scan_progress_file();
        } else {
            return Some(state.view());
        }
    }
    read_scan_progress_file()
}

fn progress_lock() -> &'static Mutex<Option<TransferProgressState>> {
    TRANSFER_PROGRESS.get_or_init(|| Mutex::new(None))
}

fn scan_progress_lock() -> &'static Mutex<Option<ScanProgressState>> {
    SCAN_PROGRESS.get_or_init(|| Mutex::new(None))
}

fn progress_file_lock() -> &'static Mutex<Option<PathBuf>> {
    PROGRESS_FILE.get_or_init(|| Mutex::new(None))
}

fn scan_progress_file_lock() -> &'static Mutex<Option<PathBuf>> {
    SCAN_PROGRESS_FILE.get_or_init(|| Mutex::new(None))
}

impl TransferProgressState {
    fn sample_speed(&mut self, now: Instant, force: bool) -> bool {
        let elapsed = now.duration_since(self.last_sample_at);
        if !force && elapsed < SPEED_SAMPLE_INTERVAL {
            return false;
        }
        let byte_delta = self.transferred_bytes.saturating_sub(self.last_bytes);
        if byte_delta == 0 {
            return false;
        }
        let millis = elapsed.as_millis().max(1);
        let sample = ((byte_delta as u128) * 1000 / millis) as u64;
        self.bytes_per_sec = ewma_speed(self.bytes_per_sec, sample);
        self.last_bytes = self.transferred_bytes;
        self.last_sample_at = now;
        true
    }

    fn view(&self) -> TransferProgressView {
        let elapsed = self.started_at.elapsed().as_secs().max(1);
        let bytes_per_sec = if self.bytes_per_sec == 0 && self.transferred_bytes > 0 {
            self.transferred_bytes / elapsed
        } else {
            self.bytes_per_sec
        };
        TransferProgressView {
            destination_id: self.destination_id.clone(),
            destination_path: self.destination_path.clone(),
            rel_path: self.rel_path.clone(),
            transferred_bytes: self.transferred_bytes,
            total_bytes: self.total_bytes,
            bytes_per_sec,
            updated_at_ms: self.updated_at_ms,
        }
    }
}

impl ScanProgressState {
    fn view(&self) -> ScanProgressView {
        ScanProgressView {
            root_path: self.root_path.clone(),
            current_path: self.current_path.clone(),
            entries_seen: self.entries_seen,
            updated_at_ms: self.updated_at_ms,
            kind: self.kind.clone(),
        }
    }
}

fn configured_progress_file() -> Option<PathBuf> {
    progress_file_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn configured_scan_progress_file() -> Option<PathBuf> {
    scan_progress_file_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

fn write_progress_file(view: &TransferProgressView) {
    let Some(path) = configured_progress_file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec(view) else {
        return;
    };
    if fs::write(&tmp, bytes).is_ok() {
        fs::rename(&tmp, &path).ok();
    }
}

fn write_scan_progress_file(view: &ScanProgressView) {
    let Some(path) = configured_scan_progress_file() else {
        return;
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let tmp = path.with_extension("json.tmp");
    let Ok(bytes) = serde_json::to_vec(view) else {
        return;
    };
    if fs::write(&tmp, bytes).is_ok() {
        fs::rename(&tmp, &path).ok();
    }
}

fn read_progress_file() -> Option<TransferProgressView> {
    let path = configured_progress_file()?;
    let bytes = fs::read(&path).ok()?;
    let view: TransferProgressView = serde_json::from_slice(&bytes).ok()?;
    let age = now_ms().saturating_sub(view.updated_at_ms);
    if age > STALE_AFTER.as_millis() {
        fs::remove_file(path).ok();
        return None;
    }
    Some(view)
}

fn read_scan_progress_file() -> Option<ScanProgressView> {
    let path = configured_scan_progress_file()?;
    let bytes = fs::read(&path).ok()?;
    let view: ScanProgressView = serde_json::from_slice(&bytes).ok()?;
    let age = now_ms().saturating_sub(view.updated_at_ms);
    if age > STALE_AFTER.as_millis() {
        fs::remove_file(path).ok();
        return None;
    }
    Some(view)
}

fn clear_progress_file() {
    if let Some(path) = configured_progress_file() {
        fs::remove_file(&path).ok();
        fs::remove_file(path.with_extension("json.tmp")).ok();
    }
}

fn clear_scan_progress_file() {
    if let Some(path) = configured_scan_progress_file() {
        fs::remove_file(&path).ok();
        fs::remove_file(path.with_extension("json.tmp")).ok();
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn ewma_speed(previous: u64, sample: u64) -> u64 {
    if previous == 0 {
        return sample;
    }
    let old_percent = 100 - EWMA_NEW_SAMPLE_PERCENT;
    ((previous as u128 * old_percent as u128 + sample as u128 * EWMA_NEW_SAMPLE_PERCENT as u128)
        / 100) as u64
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{TransferProgressState, ewma_speed};

    #[test]
    fn transfer_speed_ewma_uses_first_sample_directly() {
        assert_eq!(ewma_speed(0, 1_000), 1_000);
    }

    #[test]
    fn transfer_speed_ewma_smooths_new_samples() {
        assert_eq!(ewma_speed(1_000, 2_000), 1_350);
        assert_eq!(ewma_speed(2_000, 1_000), 1_650);
    }

    #[test]
    fn transfer_speed_can_be_sampled_when_status_is_read_later() {
        let now = Instant::now();
        let mut state = TransferProgressState {
            token: 1,
            destination_id: "dst".to_string(),
            destination_path: "/dst".to_string(),
            rel_path: "file.bin".to_string(),
            transferred_bytes: 10_000,
            total_bytes: 20_000,
            bytes_per_sec: 0,
            last_bytes: 0,
            started_at: now - Duration::from_secs(5),
            last_sample_at: now - Duration::from_secs(5),
            updated_at: now,
            updated_at_ms: 0,
        };

        assert!(state.sample_speed(now, false));
        assert_eq!(state.bytes_per_sec, 2_000);
        assert_eq!(state.last_bytes, 10_000);
    }
}
