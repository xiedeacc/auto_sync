use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const STALE_AFTER: Duration = Duration::from_secs(8);

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
    updated_at_ms: u128,
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
        let now = Instant::now();
        let elapsed = now.duration_since(state.last_sample_at);
        if elapsed >= Duration::from_millis(250) || transferred_bytes >= state.total_bytes {
            let byte_delta = transferred_bytes.saturating_sub(state.last_bytes);
            let millis = elapsed.as_millis().max(1);
            state.bytes_per_sec = ((byte_delta as u128) * 1000 / millis) as u64;
            state.last_bytes = transferred_bytes;
            state.last_sample_at = now;
        }
        state.transferred_bytes = transferred_bytes.min(state.total_bytes);
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
        state.updated_at = Instant::now();
        state.updated_at_ms = now_ms();
        write_scan_progress_file(&state.view());
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
        updated_at_ms: now_ms(),
    });
    if let Some(state) = progress.as_ref() {
        write_scan_progress_file(&state.view());
    }
    ScanProgressGuard { token }
}

pub fn current_transfer_progress() -> Option<TransferProgressView> {
    let mut progress = progress_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    if let Some(state) = progress.as_ref() {
        if state.updated_at.elapsed() > STALE_AFTER {
            *progress = None;
            clear_progress_file();
        } else {
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
