//! Push notification of status changes to peer machines.
//!
//! Polling alone leaves a remote controller up to its poll interval behind a
//! cycle advance on the source machine. Instead, any local status-affecting
//! change (cycle closed, destination status/target updated) bumps a local
//! change counter; a notifier loop pushes at most once every
//! [`PUSH_INTERVAL`] to every configured peer machine, whose UI then
//! refreshes immediately.
//!
//! Two counters keep this loop-free:
//! - `LOCAL_CHANGES` — bumped only by THIS machine's own changes; drives
//!   outgoing pushes.
//! - `STATUS_EPOCH` — bumped by local changes AND incoming peer
//!   notifications; exposed through runtime status so the UI knows to
//!   re-fetch. Incoming notifications never re-push.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use crate::core::config::{load_config, machine_is_self};
use crate::core::machines::remote_post_json;

/// Minimum spacing between outgoing pushes (user-facing latency budget).
const PUSH_INTERVAL: Duration = Duration::from_secs(2);

static LOCAL_CHANGES: AtomicU64 = AtomicU64::new(0);
static STATUS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Record that this machine's own sync state changed (cycle closed, status
/// or target updated). Cheap; call freely from state-mutation paths.
pub fn mark_local_change() {
    LOCAL_CHANGES.fetch_add(1, Ordering::Relaxed);
    STATUS_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Record an incoming peer notification: the UI should re-fetch statuses,
/// but nothing is re-pushed (prevents notification storms/loops).
pub fn note_remote_change() {
    STATUS_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// Monotonic counter the UI polls (via runtime status, already a 1s poll) to
/// detect that a full status refresh is worth doing right now.
pub fn status_epoch() -> u64 {
    STATUS_EPOCH.load(Ordering::Relaxed)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct NotifyRequest {}

#[derive(serde::Deserialize)]
struct NotifyAck {
    #[allow(dead_code)]
    #[serde(default)]
    ok: bool,
}

/// Start the notifier loop: every [`PUSH_INTERVAL`], if local changes
/// happened since the last push, notify every configured peer machine
/// (best effort — an unreachable peer just misses the hint and falls back
/// to its own polling).
pub fn spawn_notifier(config_path: std::path::PathBuf, shutdown: Arc<AtomicBool>) {
    let result = thread::Builder::new()
        .name("auto_sync_peer_notify".to_string())
        .spawn(move || {
            let mut pushed_through = 0_u64;
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(PUSH_INTERVAL);
                let current = LOCAL_CHANGES.load(Ordering::Relaxed);
                if current == pushed_through {
                    continue;
                }
                pushed_through = current;
                push_to_peers(&config_path);
            }
        });
    if let Err(err) = result {
        warn!(error = %err, "failed to spawn peer notifier thread");
    }
}

fn push_to_peers(config_path: &Path) {
    let Ok(cfg) = load_config(config_path) else {
        return;
    };
    // Parallel: one offline peer's connect timeout must not delay the others.
    thread::scope(|scope| {
        for machine in cfg
            .machines
            .iter()
            .filter(|machine| machine.id != "local" && machine.enabled)
            .filter(|machine| !machine_is_self(&cfg, machine))
        {
            scope.spawn(move || {
                if let Err(err) = remote_post_json::<_, NotifyAck>(
                    machine,
                    "/api/notify-status-changed",
                    &NotifyRequest {},
                    Duration::from_secs(3),
                ) {
                    debug!(machine = machine.id, error = %err, "peer status notification failed");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_change_bumps_both_counters_remote_bumps_only_epoch() {
        let epoch_before = status_epoch();
        let local_before = LOCAL_CHANGES.load(Ordering::Relaxed);
        mark_local_change();
        assert_eq!(status_epoch(), epoch_before + 1);
        assert_eq!(LOCAL_CHANGES.load(Ordering::Relaxed), local_before + 1);

        note_remote_change();
        assert_eq!(status_epoch(), epoch_before + 2);
        assert_eq!(
            LOCAL_CHANGES.load(Ordering::Relaxed),
            local_before + 1,
            "incoming notifications must not trigger re-pushes"
        );
    }
}
