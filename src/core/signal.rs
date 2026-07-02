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

// ---------------------------------------------------------------------------
// Watcher-armed handshake
// ---------------------------------------------------------------------------
//
// The startup change scan (mtime walk) may take minutes on a large tree. A
// file created after the walker passed its directory but before the watcher's
// marks were installed would be missed by BOTH — so the scheduler must start
// the watcher first and wait for this signal before scanning. Duplicate
// coverage (watcher event + scan hit) is harmless; a gap is not.

static WATCHER_ARMED: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();

fn watcher_armed_signal() -> &'static (Mutex<bool>, Condvar) {
    WATCHER_ARMED.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Called before (re)spawning the watcher: the previous armed state no
/// longer describes the new watcher's marks.
pub fn reset_watcher_armed() {
    let (armed, _) = watcher_armed_signal();
    *armed.lock().unwrap_or_else(|err| err.into_inner()) = false;
}

/// Called by a watcher backend once its marks/journals are actually
/// installed and events flow (also on terminal failure or when there is
/// nothing to watch — the startup scan must not wait forever).
pub fn mark_watcher_armed() {
    let (armed, condvar) = watcher_armed_signal();
    *armed.lock().unwrap_or_else(|err| err.into_inner()) = true;
    condvar.notify_all();
}

/// Block until the watcher reports armed, up to `timeout`. Returns whether
/// the watcher is armed (false = timed out; callers proceed anyway and log).
pub fn wait_watcher_armed(timeout: Duration) -> bool {
    let (armed, condvar) = watcher_armed_signal();
    let deadline = std::time::Instant::now() + timeout;
    let mut flag = armed.lock().unwrap_or_else(|err| err.into_inner());
    while !*flag {
        let now = std::time::Instant::now();
        if now >= deadline {
            return false;
        }
        let (guard, _) = condvar
            .wait_timeout(flag, deadline - now)
            .unwrap_or_else(|err| err.into_inner());
        flag = guard;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn watcher_armed_handshake_blocks_then_releases() {
        reset_watcher_armed();
        assert!(
            !wait_watcher_armed(Duration::from_millis(50)),
            "not armed yet: wait times out"
        );
        mark_watcher_armed();
        let started = Instant::now();
        assert!(wait_watcher_armed(Duration::from_secs(5)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "returns immediately once armed"
        );
        // Leave the flag armed: other tests must not block on it.
        mark_watcher_armed();
    }

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
