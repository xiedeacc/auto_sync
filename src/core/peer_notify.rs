//! Push notification of status changes to the machine that manages a source.
//!
//! Polling alone leaves a remote controller up to its poll interval behind a
//! cycle advance on the source machine. Instead, any local status-affecting
//! change (cycle closed, destination status/target updated) records the
//! affected source; a notifier loop pushes at most once every
//! [`PUSH_INTERVAL`] — and ONLY to the controller that created/manages that
//! source (`source_group.managed_by`), never to unrelated machines. A source
//! with no remote controller (created locally) is nobody else's to display,
//! so nothing is pushed for it.
//!
//! Transport: plain HTTP POST over the same pooled TCP connections the peer
//! API already uses (`/api/notify-status-changed` on the peer's web port).
//!
//! Two counters keep this loop-free:
//! - pending changed sources — recorded only by THIS machine's own changes;
//!   drive outgoing pushes.
//! - `STATUS_EPOCH` — bumped by local changes AND incoming peer
//!   notifications; exposed through runtime status so the UI knows to
//!   re-fetch. Incoming notifications never re-push.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tracing::{debug, warn};

use crate::core::config::{
    AppConfig, MachineConfig, load_config, machine_is_self, machine_matches_reference,
};
use crate::core::machines::{machine_matches_discovery_id, remote_post_json};

/// Minimum spacing between outgoing pushes (user-facing latency budget).
const PUSH_INTERVAL: Duration = Duration::from_secs(2);

static CHANGED_SOURCES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
static STATUS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Record that this machine's own sync state for `source_id` changed (cycle
/// closed, status or target updated). Cheap; call freely from state-mutation
/// paths.
pub fn mark_local_change(source_id: &str) {
    CHANGED_SOURCES
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .insert(source_id.to_string());
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

/// Start the notifier loop: every [`PUSH_INTERVAL`], drain the changed
/// sources and notify each one's managing controller (best effort — an
/// unreachable controller just misses the hint and falls back to its own
/// polling).
pub fn spawn_notifier(config_path: std::path::PathBuf, shutdown: Arc<AtomicBool>) {
    let result = thread::Builder::new()
        .name("auto_sync_peer_notify".to_string())
        .spawn(move || {
            while !shutdown.load(Ordering::SeqCst) {
                thread::sleep(PUSH_INTERVAL);
                let changed: BTreeSet<String> = std::mem::take(
                    &mut *CHANGED_SOURCES
                        .lock()
                        .unwrap_or_else(|err| err.into_inner()),
                );
                if changed.is_empty() {
                    continue;
                }
                push_to_controllers(&config_path, &changed);
            }
        });
    if let Err(err) = result {
        warn!(error = %err, "failed to spawn peer notifier thread");
    }
}

fn push_to_controllers(config_path: &Path, changed_sources: &BTreeSet<String>) {
    let Ok(cfg) = load_config(config_path) else {
        return;
    };
    // One notification per controller machine, no matter how many of its
    // sources changed.
    let mut targets: Vec<&MachineConfig> = Vec::new();
    for source_id in changed_sources {
        let Some(source) = cfg
            .source_groups
            .iter()
            .find(|source| &source.id == source_id)
        else {
            continue;
        };
        let Some(machine) = controller_machine(&cfg, &source.managed_by) else {
            continue;
        };
        if !targets
            .iter()
            .any(|existing| existing.id == machine.id && existing.host == machine.host)
        {
            targets.push(machine);
        }
    }
    // Parallel: one offline controller's connect timeout must not delay others.
    thread::scope(|scope| {
        for machine in targets {
            scope.spawn(move || {
                if let Err(err) = remote_post_json::<_, NotifyAck>(
                    machine,
                    "/api/notify-status-changed",
                    &NotifyRequest {},
                    Duration::from_secs(3),
                ) {
                    debug!(machine = machine.id, error = %err, "controller status notification failed");
                }
            });
        }
    });
}

/// Resolve `managed_by` (the controller's discovery id, or a machine
/// id/alias/host) to a configured remote machine. Returns None for sources
/// created locally (empty `managed_by`) or when the controller is this
/// machine itself.
fn controller_machine<'a>(cfg: &'a AppConfig, managed_by: &str) -> Option<&'a MachineConfig> {
    let managed_by = managed_by.trim();
    if managed_by.is_empty() {
        return None;
    }
    cfg.machines
        .iter()
        .filter(|machine| machine.id != "local" && machine.enabled)
        .filter(|machine| !machine_is_self(cfg, machine))
        .find(|machine| {
            machine_matches_reference(machine, managed_by)
                || machine_matches_discovery_id(machine, managed_by)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn local_change_records_source_and_bumps_epoch_remote_bumps_only_epoch() {
        let epoch_before = status_epoch();
        mark_local_change("src_test_notify");
        assert_eq!(status_epoch(), epoch_before + 1);
        assert!(
            CHANGED_SOURCES
                .lock()
                .unwrap()
                .contains("src_test_notify")
        );

        note_remote_change();
        assert_eq!(status_epoch(), epoch_before + 2);

        // Clean up so other tests' drains are unaffected.
        CHANGED_SOURCES.lock().unwrap().remove("src_test_notify");
    }

    #[test]
    fn controller_resolution_matches_discovery_id_and_skips_local_sources() {
        let mut cfg = AppConfig::default();
        // TEST-NET address: must not collide with the machine running the
        // tests, or machine_is_self() filters the entry out.
        cfg.machines.push(MachineConfig {
            id: "Windows".to_string(),
            alias_name: "Windows".to_string(),
            name: "DESKTOP".to_string(),
            host: "192.0.2.166".to_string(),
            port: 18765,
            ssh_user: "tiger".to_string(),
            ssh_port: 10022,
            os: "windows".to_string(),
            install_dir: PathBuf::from("D:\\code\\auto_sync"),
            enabled: true,
            manual: true,
        });

        // The controller's discovery id embeds its sanitized host and port.
        let controller =
            controller_machine(&cfg, "lan_192_0_2_166_18765_18ead87d").expect("resolved");
        assert_eq!(controller.id, "Windows");

        // Plain machine-id references still work.
        assert!(controller_machine(&cfg, "Windows").is_some());

        // Locally created sources have no controller: nothing to push.
        assert!(controller_machine(&cfg, "").is_none());
        assert!(controller_machine(&cfg, "unknown-machine").is_none());
    }
}
