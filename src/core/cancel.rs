//! Cooperative cancellation for long-running scans and syncs.
//!
//! An operation (sync pass, compare, peer-served snapshot) registers itself
//! with [`begin`], which installs a cancel token both in a global registry
//! (so an HTTP/API cancel request can find it by kind) and in a thread-local
//! (so deep loops — tree walks, transfer worker pools, chunked sends — can
//! poll it with [`check`] without threading a handle through every call).
//!
//! Worker threads do NOT inherit the thread-local; spawn sites must capture
//! [`current_token`] and re-install it with [`enter`], mirroring how the
//! compare progress context propagates.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

/// Canonical cancellation message. It travels through anyhow error chains
/// (including across the peer HTTP hop as an error body), so classification
/// is by substring — see [`error_is_cancelled`].
pub const CANCELLED_MESSAGE: &str = "cancelled by user request";

/// Operation kinds a cancel request can target.
pub const KIND_SYNC: &str = "sync";
pub const KIND_COMPARE: &str = "compare";

static NEXT_OP_ID: AtomicU64 = AtomicU64::new(1);
static REGISTRY: Mutex<Vec<ActiveOp>> = Mutex::new(Vec::new());

struct ActiveOp {
    id: u64,
    kind: String,
    /// "source_id|destination_id" labels for work tied to specific
    /// destinations (a compare has one; a source's cycle pass lists every
    /// destination it may drive). Empty for unscoped work (legacy peers),
    /// which only an untargeted request cancels.
    targets: Vec<String>,
    flag: Arc<AtomicBool>,
}

/// Cancel token installed on a thread: the shared cancelled flag plus the
/// destination target the work is for (carried so spawned workers and peer
/// requests can attribute their work to the same destination) and a shared
/// files-transferred counter the task log reads.
#[derive(Clone)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    target: Option<Arc<str>>,
    files: Arc<AtomicU64>,
}

thread_local! {
    static CURRENT: RefCell<Option<CancelToken>> = const { RefCell::new(None) };
}

/// Composes the target label carried by scoped operations. Kept in one place
/// so the UI request, the registry and peer request stamping all agree.
pub fn target_for(source_id: &str, destination_id: &str) -> String {
    format!("{source_id}|{destination_id}")
}

/// Registers a cancellable operation of the given kind and installs its token
/// on this thread until the guard drops. Nested `begin`s on one thread stack:
/// the inner operation gets its own token and the outer one is restored on
/// drop (the engine nests a per-destination scoped op inside the pass-level
/// one so a targeted cancel stops just that destination).
pub fn begin(kind: &str) -> OpGuard {
    begin_targets(kind, Vec::new())
}

/// Like [`begin`], additionally scoping the operation to one destination
/// (see [`target_for`]); a targeted cancel request only hits matching ops.
pub fn begin_target(kind: &str, target: Option<String>) -> OpGuard {
    begin_targets(kind, target.into_iter().collect())
}

/// Like [`begin`], scoping the operation to a set of destinations: a cancel
/// request targeted at ANY of them stops the whole operation (a source's
/// cycle pass serves all its destinations at once — prefetch walks and
/// transfers are shared, so it cannot stop for just one).
pub fn begin_targets(kind: &str, targets: Vec<String>) -> OpGuard {
    let id = NEXT_OP_ID.fetch_add(1, Ordering::Relaxed);
    let flag = Arc::new(AtomicBool::new(false));
    // The thread-local token carries a single unambiguous target (used to
    // stamp peer requests and progress attribution); a multi-destination op
    // has no single answer, so it carries none.
    let token_target: Option<Arc<str>> = match targets.as_slice() {
        [only] => Some(Arc::from(only.as_str())),
        _ => None,
    };
    REGISTRY
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .push(ActiveOp {
            id,
            kind: kind.to_string(),
            targets,
            flag: flag.clone(),
        });
    let token = CancelToken {
        flag,
        target: token_target,
        files: Arc::new(AtomicU64::new(0)),
    };
    let previous = CURRENT.with(|current| current.replace(Some(token)));
    OpGuard { id, previous }
}

/// Credit `count` transferred files to the operation running on this thread
/// (workers share the coordinator's token, so their transfers accumulate on
/// the same counter). No-op outside an operation.
pub fn add_synced_files(count: u64) {
    CURRENT.with(|current| {
        if let Some(token) = current.borrow().as_ref() {
            token.files.fetch_add(count, Ordering::Relaxed);
        }
    });
}

/// Files transferred so far by the operation running on this thread.
pub fn synced_files() -> u64 {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .map_or(0, |token| token.files.load(Ordering::Relaxed))
    })
}

/// True while any registered operation of `kind` is scoped to `target`; lets
/// the engine defer a destination's sync until its running compare finishes
/// (both always execute on the source's machine, so the local registry is
/// authoritative).
pub fn kind_target_active(kind: &str, target: &str) -> bool {
    REGISTRY
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .iter()
        .any(|op| op.kind == kind && op.targets.iter().any(|scoped| scoped == target))
}

pub struct OpGuard {
    id: u64,
    previous: Option<CancelToken>,
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        REGISTRY
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .retain(|op| op.id != self.id);
        CURRENT.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

/// The cancel token installed on this thread, if any. Capture it before
/// spawning worker threads and re-install with [`enter`].
pub fn current_token() -> Option<CancelToken> {
    CURRENT.with(|current| current.borrow().clone())
}

/// The destination target of the operation running on this thread, if it is
/// scoped ("source_id|destination_id"). Used to stamp outgoing peer requests
/// and progress views with the destination the work belongs to.
pub fn current_target() -> Option<String> {
    CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|token| token.target.as_ref().map(|target| target.to_string()))
    })
}

/// Installs a captured token on this (worker) thread until the guard drops.
pub fn enter(token: Option<CancelToken>) -> TokenGuard {
    let previous = CURRENT.with(|current| current.replace(token));
    TokenGuard { previous }
}

pub struct TokenGuard {
    previous: Option<CancelToken>,
}

impl Drop for TokenGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.previous.take());
    }
}

/// Cancellation poll for long loops: returns the canonical error once the
/// current operation has been cancelled. A thread with no operation installed
/// is never cancelled. One relaxed atomic load — safe to call per entry.
pub fn check() -> Result<()> {
    let cancelled = CURRENT.with(|current| {
        current
            .borrow()
            .as_ref()
            .is_some_and(|token| token.flag.load(Ordering::Relaxed))
    });
    if cancelled {
        bail!("{CANCELLED_MESSAGE}");
    }
    Ok(())
}

/// Requests cancellation of active operations. `kind = None` cancels all
/// kinds; `target = None` cancels regardless of destination. A targeted
/// request hits ONLY operations scoped to that target (strict — it must
/// never kill another source's pass on this or any propagated-to machine);
/// legacy unscoped operations require an untargeted request. Returns how
/// many operations were signalled. Idempotent; operations notice at their
/// next [`check`] and unwind with [`CANCELLED_MESSAGE`].
pub fn request(kind: Option<&str>, target: Option<&str>) -> usize {
    let registry = REGISTRY.lock().unwrap_or_else(|err| err.into_inner());
    let mut signalled = 0;
    for op in registry.iter() {
        let kind_matches = kind.is_none_or(|kind| op.kind == kind);
        let target_matches =
            target.is_none_or(|target| op.targets.iter().any(|scoped| scoped == target));
        if kind_matches && target_matches {
            op.flag.store(true, Ordering::Relaxed);
            signalled += 1;
        }
    }
    signalled
}

/// The kinds of operations currently registered (deduplicated), so callers
/// can report what is actually cancellable.
pub fn active_kinds() -> Vec<String> {
    let registry = REGISTRY.lock().unwrap_or_else(|err| err.into_inner());
    let mut kinds: Vec<String> = registry.iter().map(|op| op.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// True when the error (anywhere in its chain, including peer HTTP error
/// bodies) is a user cancellation. Cancelled work must not be retried and is
/// reported as "cancelled", not as a destination fault.
pub fn error_is_cancelled(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.to_string().contains(CANCELLED_MESSAGE))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global and lib tests run in parallel, so these
    // tests use unique kinds (and never request(None)) to avoid cancelling
    // sync/compare operations that OTHER tests have registered.

    #[test]
    fn check_passes_without_operation_and_fails_after_request() {
        assert!(check().is_ok());
        let guard = begin("test-kind-basic");
        assert!(check().is_ok());
        assert_eq!(request(Some("test-kind-basic"), None), 1);
        let err = check().unwrap_err();
        assert!(error_is_cancelled(&err));
        drop(guard);
        assert!(check().is_ok(), "token uninstalled after guard drop");
    }

    #[test]
    fn request_by_kind_only_hits_matching_operations() {
        let _outer = begin("test-kind-outer");
        let inner_flag = {
            let _inner = begin("test-kind-inner");
            current_token().unwrap().flag
        };
        // Inner guard dropped: its kind is no longer registered.
        assert_eq!(request(Some("test-kind-inner"), None), 0);
        assert!(!inner_flag.load(Ordering::Relaxed));
        assert!(check().is_ok(), "outer op untouched by other-kind cancel");
        assert_eq!(request(Some("test-kind-outer"), None), 1);
        assert!(check().is_err(), "outer token restored and cancelled");
    }

    #[test]
    fn targeted_request_is_strict_and_multi_target_ops_match_any_of_theirs() {
        let _scoped = begin_target("test-kind-target", Some(target_for("srcA", "dst1")));
        assert_eq!(
            current_target().as_deref(),
            Some("srcA|dst1"),
            "single-target op exposes its target on the thread"
        );
        // A different destination's targeted cancel must not touch it...
        assert_eq!(
            request(Some("test-kind-target"), Some("srcB|dst1")),
            0,
            "strict matching: other targets and unscoped-op fallbacks are out"
        );
        assert!(check().is_ok());
        // ...while its own target does.
        assert_eq!(request(Some("test-kind-target"), Some("srcA|dst1")), 1);
        assert!(check().is_err());

        let _pass = begin_targets(
            "test-kind-multi",
            vec![target_for("srcC", "dst1"), target_for("srcC", "dst2")],
        );
        assert_eq!(
            current_target(),
            None,
            "multi-target op has no single attribution target"
        );
        assert_eq!(request(Some("test-kind-multi"), Some("srcC|dst2")), 1);
        assert!(check().is_err(), "any listed target cancels the whole op");
    }

    #[test]
    fn worker_threads_see_cancellation_through_entered_token() {
        let _op = begin("test-kind-worker");
        request(Some("test-kind-worker"), None);
        let token = current_token();
        let cancelled = std::thread::spawn(move || {
            let _enter = enter(token);
            check().is_err()
        })
        .join()
        .unwrap();
        assert!(cancelled);
    }
}
