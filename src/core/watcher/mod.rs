#[cfg(target_os = "linux")]
pub mod fanotify;

#[cfg(not(target_os = "linux"))]
pub mod fanotify {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use crate::core::config::AppConfig;

    pub fn spawn_fanotify_thread(
        _cfg: AppConfig,
        _db_path: PathBuf,
        shutdown: Arc<AtomicBool>,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            tracing::info!(
                "fanotify is only available on Linux; realtime sources use periodic reconciliation"
            );
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
            }
        })
    }
}
