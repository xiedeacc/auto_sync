//! Cross-thread wakeup for the scheduler loop. Watchers signal here after
//! persisting an event so the scheduler reacts immediately instead of waiting
//! out its polling interval — "realtime" latency drops from up-to-5s to
//! milliseconds while the poll remains as a fallback heartbeat.

use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

static SIGNAL: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();

fn signal() -> &'static (Mutex<bool>, Condvar) {
    SIGNAL.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Wake the scheduler now (e.g. a watcher just recorded an event). Cheap and
/// safe to call from any thread; coalesces with pending wakeups.
pub fn notify_scheduler() {
    let (pending, condvar) = signal();
    *pending.lock().unwrap_or_else(|err| err.into_inner()) = true;
    condvar.notify_all();
}

/// Sleep until notified or `timeout` elapses, consuming the pending flag. A
/// notification that arrived while the scheduler was busy is not lost — the
/// next wait returns immediately.
pub fn wait_for_activity(timeout: Duration) {
    let (pending, condvar) = signal();
    let mut flag = pending.lock().unwrap_or_else(|err| err.into_inner());
    if !*flag {
        let (guard, _) = condvar
            .wait_timeout(flag, timeout)
            .unwrap_or_else(|err| err.into_inner());
        flag = guard;
    }
    *flag = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn pending_notification_wakes_immediately() {
        notify_scheduler();
        let started = Instant::now();
        wait_for_activity(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(1));
        // Flag consumed: the next wait times out.
        let started = Instant::now();
        wait_for_activity(Duration::from_millis(50));
        assert!(started.elapsed() >= Duration::from_millis(40));
    }
}
