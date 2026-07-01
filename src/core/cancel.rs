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
    flag: Arc<AtomicBool>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Registers a cancellable operation of the given kind and installs its token
/// on this thread until the guard drops. Nested `begin`s on one thread stack:
/// the inner operation gets its own token and the outer one is restored on
/// drop (in practice entry points don't nest — local snapshot calls run under
/// the enclosing sync/compare operation and must NOT call `begin` again).
pub fn begin(kind: &str) -> OpGuard {
    let id = NEXT_OP_ID.fetch_add(1, Ordering::Relaxed);
    let flag = Arc::new(AtomicBool::new(false));
    REGISTRY
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .push(ActiveOp {
            id,
            kind: kind.to_string(),
            flag: flag.clone(),
        });
    let previous = CURRENT.with(|current| current.replace(Some(flag)));
    OpGuard { id, previous }
}

pub struct OpGuard {
    id: u64,
    previous: Option<Arc<AtomicBool>>,
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
pub fn current_token() -> Option<Arc<AtomicBool>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Installs a captured token on this (worker) thread until the guard drops.
pub fn enter(token: Option<Arc<AtomicBool>>) -> TokenGuard {
    let previous = CURRENT.with(|current| current.replace(token));
    TokenGuard { previous }
}

pub struct TokenGuard {
    previous: Option<Arc<AtomicBool>>,
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
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    });
    if cancelled {
        bail!("{CANCELLED_MESSAGE}");
    }
    Ok(())
}

/// Requests cancellation of active operations. `kind = None` cancels all;
/// otherwise only operations registered under that kind. Returns how many
/// operations were signalled. Idempotent; operations notice at their next
/// [`check`] and unwind with [`CANCELLED_MESSAGE`].
pub fn request(kind: Option<&str>) -> usize {
    let registry = REGISTRY.lock().unwrap_or_else(|err| err.into_inner());
    let mut signalled = 0;
    for op in registry.iter() {
        if kind.is_none_or(|kind| op.kind == kind) {
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
        assert_eq!(request(Some("test-kind-basic")), 1);
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
            current_token().unwrap()
        };
        // Inner guard dropped: its kind is no longer registered.
        assert_eq!(request(Some("test-kind-inner")), 0);
        assert!(!inner_flag.load(Ordering::Relaxed));
        assert!(check().is_ok(), "outer op untouched by other-kind cancel");
        assert_eq!(request(Some("test-kind-outer")), 1);
        assert!(check().is_err(), "outer token restored and cancelled");
    }

    #[test]
    fn worker_threads_see_cancellation_through_entered_token() {
        let _op = begin("test-kind-worker");
        request(Some("test-kind-worker"));
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
