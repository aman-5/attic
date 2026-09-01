//! `attic-incremental` — Phase 2: Incremental Correctness and Freshness.
//!
//! Pipeline (every stage bounded; invalidation ≠ recomputation):
//!
//! ```text
//! native watcher events (hints only)
//!     → early ignore/security filter          [events]
//!     → normalize                             [events]
//!     → bounded coalescer (debounce)          [coalesce]
//!     → verify actual source state            [changeset]
//!     → ChangeSet
//!     → invalidation DAG                      [invalidation]  (cheap, sync)
//!     → bounded task queue                    [scheduler]
//!     → scoped recompute via Phase 1B/1C/1A   [attic_indexing::index_changes]
//!     → fresh CURRENT state
//! ```
//!
//! Watcher events are NEVER source truth: canonical mutations happen only
//! after [`changeset::verify`] re-hashes actual file content.  Queue overflow,
//! watcher errors, or event loss mark affected state UNKNOWN and trigger a
//! bounded authoritative reconciliation ([`recovery::reconcile_repository`]).

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod changeset;
pub mod coalesce;
pub mod events;
pub mod freshness;
pub mod invalidation;
pub mod recovery;
pub mod scheduler;
pub mod service;

pub use changeset::{FileChange, VerifiedChangeSet};
pub use coalesce::{CoalescedChange, EventCoalescer};
pub use events::{FsEventKind, NormalizedEvent};
pub use freshness::assert_transition;
pub use recovery::{
    ReconcileReport, RecoveryReport, plan_offline_refresh, reconcile_repository,
    record_clean_shutdown_marker, run_startup_recovery,
};
pub use scheduler::{
    SchedulerConfig, SchedulerHandle, TaskOrigin, run_next_task_synchronously, spawn_scheduler,
};
pub use service::{
    DEFAULT_QUIET_MS, FallbackGuard, IncrementalService, IncrementalWatch, ServiceStatus,
    StepReport, WatchMode,
};

use thiserror::Error;

/// Errors produced by the incremental subsystem.
#[derive(Debug, Error)]
pub enum IncrementalError {
    /// Storage / writer-queue failure.
    #[error("storage error: {0}")]
    Storage(#[from] attic_storage::StorageError),
    /// Scoped indexing failure.
    #[error("indexing error: {0}")]
    Indexing(#[from] attic_indexing::IndexError),
    /// Discovery walk failure (reconciliation).
    #[error("discovery error: {0}")]
    Discovery(#[from] attic_discovery::DiscoveryError),
    /// I/O failure while verifying source content.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A freshness transition violated the contract.
    #[error("illegal freshness transition: {0}")]
    Freshness(#[from] freshness::FreshnessTransitionError),
    /// Repository must be bootstrapped with a full index first.
    #[error("repository not bootstrapped: {0}")]
    NotBootstrapped(String),
    /// Scheduler could not start (zero or failed worker threads).
    #[error("scheduler startup failed: {0}")]
    Scheduler(String),
}

/// Monotonic-ish wall clock in microseconds (failure-tolerant).
pub(crate) fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Run one read/write function on the coordinated writer connection and get
/// its result back.
///
/// The Phase 1A `WriterQueueHandle::send` deliberately returns no payload;
/// this helper carries values/errors out through a shared slot using the same
/// pattern as [`attic_storage::submit_index_publication`].
pub fn run_on_writer<T, F>(
    writer: &attic_storage::WriterQueueHandle,
    f: F,
) -> Result<T, IncrementalError>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Connection) -> Result<T, attic_storage::StorageError> + Send + 'static,
{
    // The writer closure's own return value is the *only* signal
    // `WriterQueueHandle::send` uses to decide COMMIT vs ROLLBACK, so it must
    // mirror `f`'s own Ok/Err exactly: an `Err` from `f` must come back out
    // of this closure as `Err`, never be swallowed into `Ok(())`, or a failed
    // mutation can still commit while the caller is told it failed.
    //
    // `slot` exists only to carry the *successful* generic payload `v` out of
    // the `'static` closure (whose own return type is fixed at
    // `Result<(), StorageError>`, so `v` cannot travel through it directly).
    // It must never be used to smuggle an error into a fabricated `Ok(())`.
    let slot: std::sync::Arc<std::sync::Mutex<Option<T>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let sink = std::sync::Arc::clone(&slot);

    writer
        .send(move |conn| match f(conn) {
            Ok(v) => match sink.lock() {
                Ok(mut guard) => {
                    *guard = Some(v);
                    Ok(())
                }
                Err(_) => {
                    // `f` already mutated the in-progress transaction, but
                    // there is no way left to hand `v` back to the caller.
                    // Committing here would leave the DB holding a change
                    // the caller can never observe (and may retry,
                    // duplicating the work), so force a rollback instead.
                    Err(attic_storage::StorageError::MutexPoisoned(
                        "run_on_writer result slot poisoned after f(conn) succeeded".into(),
                    ))
                }
            },
            // Never translate a failing `f` into `Ok(())`: return its
            // original error so the writer rolls back and the caller
            // receives the real, untouched failure.
            Err(e) => Err(e),
        })
        .map_err(IncrementalError::Storage)?;

    // Reached only when the writer closure returned `Ok(())`, i.e. the
    // mutation committed and `slot` was populated under a healthy lock.
    match slot.lock() {
        Ok(mut guard) => guard.take().ok_or_else(|| {
            IncrementalError::Storage(attic_storage::StorageError::Worker(
                "writer closure committed without producing a result".into(),
            ))
        }),
        Err(_) => Err(IncrementalError::Storage(
            attic_storage::StorageError::MutexPoisoned(
                "run_on_writer result slot poisoned after commit".into(),
            ),
        )),
    }
}
