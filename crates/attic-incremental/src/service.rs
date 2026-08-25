//! `IncrementalService` — wires the pipeline stages together.
//!
//! Two usage modes:
//! - **Library/deterministic**: call [`IncrementalService::ingest`],
//!   [`IncrementalService::apply_pending`] with explicit timestamps — used by
//!   the test suite (no sleeps, no races).
//! - **Server mode** ([`IncrementalService::spawn_watcher`]): a real
//!   `notify-debouncer-full` watcher feeds normalized events through the same
//!   bounded channel; a pump thread drives drain/verify/apply/schedule.
//!
//! Invariants enforced here:
//! - every queue is bounded; overflow ⇒ `reconciliation_required = true`
//!   and affected state goes UNKNOWN (never silently CURRENT);
//! - verification precedes ANY canonical mutation;
//! - unaffected CURRENT artifacts keep serving while affected ones refresh.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tracing::{debug, warn};

use attic_discovery::DiscoveryPolicy;
use attic_storage::{DbPool, IncrementalTaskPayload, WriterQueueHandle};

use crate::changeset::{self, DbSnapshotSource, VerifiedChangeSet};
use crate::coalesce::{CoalescedChange, EventCoalescer};
use crate::events;
use crate::invalidation::AppliedInvalidation;
use crate::scheduler::{ScheduleOutcome, dedup_key};
use crate::{IncrementalError, invalidation, recovery, run_on_writer, scheduler};

/// Default debounce quiet period (OQ-005 resolution: 500 ms default).
pub const DEFAULT_QUIET_MS: u64 = 500;
/// Default bound on simultaneously-pending paths in the coalescer.
pub const DEFAULT_COALESCE_CAPACITY: usize = 8192;

/// Shared counters / flags for observability + MCP status.
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    /// Raw normalized events accepted.
    pub events_ingested: AtomicU64,
    /// Hints dropped because the coalescer was full.
    pub hints_dropped: AtomicU64,
    /// Watcher error batches received (event-loss signal).
    pub watcher_errors: AtomicU64,
    /// Verified change sets applied.
    pub changesets_applied: AtomicU64,
    /// Whether event loss requires authoritative reconciliation.
    pub reconciliation_required: AtomicBool,
}

/// The incremental service core.
///
/// Owns the bounded coalescer; DB access flows through the Phase 1A pool +
/// coordinated writer exactly like every other subsystem.
pub struct IncrementalService {
    root: PathBuf,
    policy: DiscoveryPolicy,
    repo_id: Mutex<Option<String>>,
    coalescer: Arc<Mutex<EventCoalescer>>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    quiet_ms: u64,
}

impl IncrementalService {
    /// Create a service for one repository root.
    pub fn new(root: &Path, policy: DiscoveryPolicy) -> Self {
        Self {
            root: root.to_path_buf(),
            policy,
            repo_id: Mutex::new(None),
            coalescer: Arc::new(Mutex::new(EventCoalescer::new(
                DEFAULT_QUIET_MS,
                DEFAULT_COALESCE_CAPACITY,
            ))),
            metrics: Arc::new(ServiceMetrics::default()),
            quiet_ms: DEFAULT_QUIET_MS,
        }
    }

    /// Override the debounce quiet period (tests use tiny values; server may
    /// tune via configuration later).
    pub fn with_quiet_period_ms(mut self, ms: u64) -> Self {
        self.quiet_ms = ms;
        self.coalescer = Arc::new(Mutex::new(EventCoalescer::new(
            ms,
            DEFAULT_COALESCE_CAPACITY,
        )));
        self
    }

    fn resolve_repo(&self, pool: &DbPool) -> Result<String, IncrementalError> {
        if let Some(id) = self.repo_id.lock().ok().and_then(|g| g.clone()) {
            return Ok(id);
        }
        let root_str = self.root.to_string_lossy().to_string();
        let id = pool
            .with_reader(|c| attic_storage::lookup_repository_by_root_path(c, &root_str))?
            .ok_or_else(|| IncrementalError::NotBootstrapped(root_str))?;
        let s = id.to_string_repr();
        if let Ok(mut g) = self.repo_id.lock() {
            *g = Some(s.clone());
        }
        Ok(s)
    }

    /// Feed raw normalized events into the bounded pipeline.
    ///
    /// Returns `false` if any event had to be shed (overflow) — callers must
    /// treat state as uncertain until reconciliation completes.
    pub fn ingest(&self, evs: &[events::NormalizedEvent]) -> bool {
        let now_ms = crate::now_micros() as u64;
        let mut all_accepted = true;
        if let Ok(mut c) = self.coalescer.lock() {
            for ev in evs {
                self.metrics.events_ingested.fetch_add(1, Ordering::Relaxed);
                if !c.push(ev, now_ms) {
                    all_accepted = false;
                    self.metrics.hints_dropped.fetch_add(1, Ordering::Relaxed);
                    self.metrics
                        .reconciliation_required
                        .store(true, Ordering::SeqCst);
                }
            }
            if c.overflowed() {
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
            }
        } else {
            all_accepted = false;
            self.metrics
                .reconciliation_required
                .store(true, Ordering::SeqCst);
        }
        all_accepted
    }

    /// Drain coalesced hints whose quiet period elapsed at `now_override_ms`
    /// (`None` = wall clock).  Pure — no I/O.
    pub fn drain_due(&self, now_override_ms: Option<u64>) -> Vec<CoalescedChange> {
        let now_ms = now_override_ms.unwrap_or_else(|| crate::now_micros() as u64);
        match self.coalescer.lock() {
            Ok(mut c) => c.drain_due(now_ms),
            Err(_) => Vec::new(),
        }
    }

    /// Force-flush every pending hint regardless of quiet period.
    pub fn flush_all(&self) -> Vec<CoalescedChange> {
        match self.coalescer.lock() {
            Ok(mut c) => c.flush_all(),
            Err(_) => Vec::new(),
        }
    }

    /// Verify pending hints against real disk + persisted state.
    pub fn verify(
        &self,
        pool: &DbPool,
        ops: Vec<CoalescedChange>,
    ) -> Result<VerifiedChangeSet, IncrementalError> {
        let repo_id = self.resolve_repo(pool)?;
        let typed: attic_core::RepositoryId = repo_id
            .parse()
            .map_err(|_| IncrementalError::NotBootstrapped(repo_id.clone()))?;
        let source = DbSnapshotSource::new(pool, typed);
        Ok(changeset::verify(&self.root, ops, &source))
    }

    /// Full synchronous step: drain → verify → invalidate → schedule.
    ///
    /// This is the deterministic tests' primary driver; the watch-pump calls
    /// it too (with wall-clock time).
    pub fn apply_pending(
        &self,
        pool: &DbPool,
        writer: &WriterQueueHandle,
        now_override_ms: Option<u64>,
    ) -> Result<StepReport, IncrementalError> {
        let ops = self.drain_due(now_override_ms);
        if ops.is_empty() {
            return Ok(StepReport::default());
        }
        self.apply_operations(pool, writer, ops)
    }

    /// Apply an ALREADY-VERIFIED change set directly.
    ///
    /// Used by reconciliation flows whose facts come from the authoritative
    /// discovery walk (e.g. discovery-policy exclusions where the file still
    /// exists on disk); per-hint disk re-verification would wrongly cancel
    /// them, so it is skipped by construction here.
    pub fn apply_verified_change_set(
        &self,
        pool: &DbPool,
        writer: &WriterQueueHandle,
        cs: &VerifiedChangeSet,
    ) -> Result<StepReport, IncrementalError> {
        let mut report = StepReport {
            verified_upserts: cs.upserts.len(),
            verified_deletes: cs.deletes.len(),
            verified_renames: cs.renames.len(),
            ..Default::default()
        };

        if cs.policy_changed {
            recovery::schedule_reconciliation(writer)?;
            report.policy_rediscovery_scheduled = true;
        }
        if cs.upserts.is_empty() && cs.deletes.is_empty() && cs.renames.is_empty() {
            return Ok(report);
        }

        let repo_id = self.resolve_repo(pool)?;

        let counts = invalidation::apply_invalidation(
            writer,
            &repo_id,
            cs,
            attic_core::InvalidationCause::SourceChanged,
            crate::now_micros(),
        )?;
        report.invalidation = counts;

        let payload = IncrementalTaskPayload {
            dedup_key: dedup_key(cs),
            upserts: cs.upserts.clone(),
            deletes: cs.deletes.clone(),
            renames: cs.renames.clone(),
            from_reconciliation: true,
        };
        match scheduler::schedule_incremental(
            writer,
            &repo_id,
            &payload,
            scheduler::PRIORITY_RECONCILE,
            DEFAULT_TASK_MAX_PENDING,
        )? {
            ScheduleOutcome::Queued => report.task_queued = true,
            ScheduleOutcome::Deduplicated => report.task_deduplicated = true,
            ScheduleOutcome::Saturated => {
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
                mark_paths_unknown(writer, &repo_id, cs)?;
                recovery::schedule_reconciliation(writer)?;
                report.queue_saturated = true;
            }
        }
        self.metrics
            .changesets_applied
            .fetch_add(1, Ordering::Relaxed);
        Ok(report)
    }

    /// Apply an explicit operation list (deterministic entry point for tests).
    pub fn apply_operations(
        &self,
        pool: &DbPool,
        writer: &WriterQueueHandle,
        ops: Vec<CoalescedChange>,
    ) -> Result<StepReport, IncrementalError> {
        if ops.is_empty() {
            return Ok(StepReport::default());
        }

        let cs = self.verify(pool, ops)?;
        let mut report = StepReport {
            verified_upserts: cs.upserts.len(),
            verified_deletes: cs.deletes.len(),
            verified_renames: cs.renames.len(),
            ..Default::default()
        };

        // Policy input changed? → targeted rediscovery via RECONCILIATION
        // task; scoped handling still applies to what we know changed.
        if cs.policy_changed {
            debug!("discovery-policy input changed; scheduling targeted rediscovery");
            recovery::schedule_reconciliation(writer)?;
            report.policy_rediscovery_scheduled = true;
        }

        if cs.upserts.is_empty() && cs.deletes.is_empty() && cs.renames.is_empty() {
            debug!("all hints verified as no-ops; nothing invalidated");
            return Ok(report);
        }

        let repo_id = self.resolve_repo(pool)?;

        // ── Stage 1: cheap synchronous invalidation (DAG propagation) ──────
        let counts = invalidation::apply_invalidation(
            writer,
            &repo_id,
            &cs,
            attic_core::InvalidationCause::SourceChanged,
            crate::now_micros(),
        )?;
        report.invalidation = counts;

        // ── Stage 2: bounded scheduled recomputation (separate!) ───────────
        let payload = IncrementalTaskPayload {
            dedup_key: dedup_key(&cs),
            upserts: cs.upserts.clone(),
            deletes: cs.deletes.clone(),
            renames: cs.renames.clone(),
            from_reconciliation: false,
        };
        match scheduler::schedule_incremental(
            writer,
            &repo_id,
            &payload,
            scheduler::PRIORITY_USER_EDIT,
            DEFAULT_TASK_MAX_PENDING,
        )? {
            ScheduleOutcome::Queued => report.task_queued = true,
            ScheduleOutcome::Deduplicated => report.task_deduplicated = true,
            ScheduleOutcome::Saturated => {
                // Queue saturated: mark affected paths UNKNOWN so nothing
                // falsely claims CURRENT, then demand authoritative rescan.
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
                mark_paths_unknown(writer, &repo_id, &cs)?;
                recovery::schedule_reconciliation(writer)?;
                report.queue_saturated = true;
            }
        }
        self.metrics
            .changesets_applied
            .fetch_add(1, Ordering::Relaxed);
        Ok(report)
    }

    /// React to watcher errors (possible event loss): require reconciliation.
    pub fn on_watcher_error(&self, detail: &str) {
        self.metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .reconciliation_required
            .store(true, Ordering::SeqCst);
        warn!(detail, "watcher error batch — reconciliation required");
    }

    /// Whether event loss / saturation demands an authoritative rescan.
    pub fn reconciliation_required(&self) -> bool {
        self.metrics.reconciliation_required.load(Ordering::SeqCst)
    }

    /// Clear the flag after a successful authoritative pass.
    pub fn clear_reconciliation_flag(&self) {
        self.metrics
            .reconciliation_required
            .store(false, Ordering::SeqCst);
    }

    /// The configured repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The active discovery policy (borrowed).
    pub fn policy(&self) -> &DiscoveryPolicy {
        &self.policy
    }

    /// Debounce window in milliseconds.
    pub fn quiet_ms(&self) -> u64 {
        self.quiet_ms
    }

    /// Spawn a real OS watcher + bounded pump feeding this service.
    ///
    /// The DB endpoints are explicit arguments (no hidden globals).  The
    /// returned guard owns the watcher; dropping it stops watching and joins
    /// the pump thread.
    pub fn spawn_watcher(
        self: &Arc<Self>,
        pool: DbPool,
        writer: WriterQueueHandle,
    ) -> Result<WatcherGuard<notify_debouncer_full::RecommendedCache>, IncrementalError> {
        use std::sync::mpsc::SyncSender;
        use std::time::Duration;

        enum PumpMsg {
            Batch(notify_debouncer_full::DebounceEventResult),
        }

        // Bounded raw-event queue (contract §13).
        let (tx, rx): (SyncSender<PumpMsg>, std::sync::mpsc::Receiver<PumpMsg>) =
            std::sync::mpsc::sync_channel(RAW_EVENT_QUEUE_BOUND);

        let quiet = Duration::from_millis(self.quiet_ms);
        let tx_err = tx.clone();
        let mut debouncer = notify_debouncer_full::new_debouncer(
            quiet,
            None,
            move |res: notify_debouncer_full::DebounceEventResult| {
                // If the bounded pipe is full we MUST NOT drop silently:
                // surface it through the error counter path instead.
                if tx_err.send(PumpMsg::Batch(res)).is_err() {
                    // Receiver gone; nothing left to protect.
                }
            },
        )
        .map_err(|e| IncrementalError::Io(std::io::Error::other(e.to_string())))?;

        debouncer
            .watch(
                self.root.clone(),
                notify_debouncer_full::notify::RecursiveMode::Recursive,
            )
            .map_err(|e| IncrementalError::Io(std::io::Error::other(e.to_string())))?;

        let svc = Arc::clone(self);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_pump = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("attic-watch-pump".into())
            .spawn(move || {
                loop {
                    if stop_pump.load(Ordering::SeqCst) {
                        break;
                    }
                    match rx.recv_timeout(Duration::from_millis(250)) {
                        Ok(PumpMsg::Batch(Ok(debounced))) => {
                            let mut normalized = Vec::new();
                            for ev in &debounced {
                                normalized.extend(events::normalize_debounced(ev, &svc.root));
                            }
                            if !normalized.is_empty() {
                                svc.ingest(&normalized);
                                if let Err(e) = svc.apply_pending(&pool, &writer, None) {
                                    warn!(error = %e, "watch pump apply failed");
                                }
                            }
                        }
                        Ok(PumpMsg::Batch(Err(errors))) => {
                            for e in errors {
                                svc.on_watcher_error(&e.to_string());
                            }
                            // Event loss demands an authoritative rescan task.
                            let _ = recovery::schedule_reconciliation(&writer);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(|e| IncrementalError::Io(std::io::Error::other(e.to_string())))?;

        Ok(WatcherGuard {
            _debouncer: Mutex::new(Some(debouncer)),
            stop,
            pump: Some(handle),
        })
    }

    /// Aggregate status snapshot for the MCP `status` tool.
    pub fn status_snapshot(&self, pool: &DbPool) -> Result<ServiceStatus, IncrementalError> {
        let freshness = pool.with_reader(attic_storage::get_freshness_totals)?;
        let tasks = scheduler::queue_status(pool)?;
        Ok(ServiceStatus {
            events_ingested: self.metrics.events_ingested.load(Ordering::Relaxed),
            hints_dropped: self.metrics.hints_dropped.load(Ordering::Relaxed),
            watcher_errors: self.metrics.watcher_errors.load(Ordering::Relaxed),
            reconciliation_required: self.reconciliation_required(),
            freshness,
            tasks,
        })
    }
}

/// Bound of the raw watcher→pump channel (normalized batches).
const RAW_EVENT_QUEUE_BOUND: usize = 4096;
/// Default scheduler saturation threshold used by the service step.
const DEFAULT_TASK_MAX_PENDING: usize = 4096;

/// Concrete watcher-guard type used by the server binary.
pub type DefaultWatcherGuard = WatcherGuard<notify_debouncer_full::RecommendedCache>;

/// Stop handle owning the watcher + pump thread.
pub struct WatcherGuard<C: notify_debouncer_full::FileIdCache> {
    _debouncer: Mutex<
        Option<
            notify_debouncer_full::Debouncer<notify_debouncer_full::notify::RecommendedWatcher, C>,
        >,
    >,
    stop: Arc<AtomicBool>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl<C: notify_debouncer_full::FileIdCache> Drop for WatcherGuard<C> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the debouncer stops its internal threads first.
        if let Ok(mut g) = self._debouncer.lock() {
            *g = None;
        }
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

fn mark_paths_unknown(
    writer: &WriterQueueHandle,
    repo_id: &str,
    cs: &VerifiedChangeSet,
) -> Result<(), IncrementalError> {
    let typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| IncrementalError::NotBootstrapped(repo_id.to_owned()))?;
    let paths: Vec<String> = cs.touched_paths().into_iter().collect();
    run_on_writer(writer, move |conn| {
        for p in &paths {
            if let Some(snap) = attic_storage::lookup_occurrence_snapshot(conn, &typed, p)? {
                conn.execute(
                    "UPDATE core_file_occurrences SET freshness_state = 'UNKNOWN'
                      WHERE id = ?1 AND freshness_state = 'CURRENT'",
                    [&snap.id],
                )?;
            }
        }
        Ok(())
    })
}

/// One pipeline-step summary (observability + tests).
#[derive(Debug, Default, Clone)]
pub struct StepReport {
    /// Verified upsert count.
    pub verified_upserts: usize,
    /// Verified delete count.
    pub verified_deletes: usize,
    /// Verified rename pair count.
    pub verified_renames: usize,
    /// Invalidation DAG counters.
    pub invalidation: AppliedInvalidation,
    /// A recomputation task was queued.
    pub task_queued: bool,
    /// An identical task was already pending (dedup hit).
    pub task_deduplicated: bool,
    /// Pending queue was saturated (state marked UNKNOWN + reconcile).
    pub queue_saturated: bool,
    /// A targeted rediscovery was scheduled (.gitignore/policy change).
    pub policy_rediscovery_scheduled: bool,
}

/// JSON-ready service status for MCP `status`.
#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    /// Normalized events accepted since start.
    pub events_ingested: u64,
    /// Hints shed by the bounded coalescer.
    pub hints_dropped: u64,
    /// Watcher error batches (potential event loss).
    pub watcher_errors: u64,
    /// Whether authoritative reconciliation is required.
    pub reconciliation_required: bool,
    /// Occurrence freshness totals.
    pub freshness: attic_storage::FreshnessTotals,
    /// Task queue counters.
    pub tasks: scheduler::QueueStatus,
}
