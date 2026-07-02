use crate::core::config::{SourceGroupConfig, machine_id_or_local};

#[cfg(target_os = "linux")]
pub mod fanotify;

#[cfg(target_os = "windows")]
pub mod windows_usn;

#[cfg(target_os = "linux")]
pub use fanotify::spawn_source_watcher_thread;

#[cfg(target_os = "windows")]
pub use windows_usn::spawn_source_watcher_thread;

/// Whether this platform's watcher backend can observe changes made while
/// the daemon was NOT running. Windows USN reads a persistent OS journal (and
/// its stored cursor detects journal gaps, recording `rescan_required`), so
/// downtime is covered. fanotify has no journal: whatever happened while the
/// process was down is unobservable, and the daemon raises a restart notice
/// for the user instead of silently full-scanning the tree on every start.
pub fn watcher_covers_downtime() -> bool {
    cfg!(windows)
}

/// Sources whose changes this machine is responsible for watching (used to
/// decide which sources get a restart notice).
pub fn source_is_watched_here(source: &SourceGroupConfig) -> bool {
    source.enabled
        && machine_id_or_local(&source.machine_id) == "local"
        && source.destinations.iter().any(|dst| dst.enabled)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn spawn_source_watcher_thread(
    _cfg: crate::core::config::AppConfig,
    _db_path: std::path::PathBuf,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    let _ = (PathBuf::new(), Arc::<AtomicBool>::clone(&shutdown));
    thread::spawn(move || {
        tracing::info!(
            "realtime source watcher is not available on this platform; using periodic reconciliation"
        );
        crate::core::signal::mark_watcher_armed();
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
    })
}
