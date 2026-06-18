#[cfg(target_os = "linux")]
pub mod fanotify;

#[cfg(target_os = "windows")]
pub mod windows_usn;

#[cfg(target_os = "linux")]
pub use fanotify::spawn_source_watcher_thread;

#[cfg(target_os = "windows")]
pub use windows_usn::spawn_source_watcher_thread;

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
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
        }
    })
}
