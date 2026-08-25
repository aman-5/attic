//! `IncrementalService` — wires the pipeline stages together.
//!
//! Two usage modes:
//! - **Library/deterministic**: call [`IncrementalService::ingest`] +
//!   [`IncrementalService::apply_pending`] with explicit timestamps — used by
//!   the test suite (no sleeps, no races).
//! - **Server mode** ([`IncrementalService::start_incremental_watch`]): a real
//!   `notify-debouncer-full` watcher feeds a **bounded, saturation-aware**
//!   channel; the pump ticks independently of batch arrival so pending hints
//!   are flushed as soon as the quiet period elapses.  If the native watcher
//!   cannot start, an actual bounded periodic authoritative reconciliation
//!   loop runs instead ([`WatchMode::PeriodicReconciliation`]) — there is no
//!   fake "reconciliation-only mode".
//!
//! Invariants enforced here:
//! - every queue is bounded; overflow ⇒ `reconciliation_required = true`
//!   and affected state goes UNKNOWN (never silently CURRENT);
//! - verification precedes ANY canonical mutation; only verified absence
//!   (`NotFound`) may delete — read/hash failures degrade to UNKNOWN;
//! - unaffected CURRENT artifacts keep serving while affected ones refresh.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, warn};

use attic_discovery::DiscoveryPolicy;
use attic_storage::{DbPool, IncrementalTaskPayload, WriterQueueHandle};

use crate::changeset::{self, DbSnapshotSource, VerifiedChangeSet};
use crate::coalesce::{CoalescedChange, EventCoalescer};
use crate::events::{self, EventFilter};
use crate::invalidation::AppliedInvalidation;
use crate::scheduler::{ScheduleOutcome, dedup_key};
use crate::{IncrementalError, invalidation, recovery, run_on_writer, scheduler};

/// Default debounce quiet period in **milliseconds** (OQ-005 resolution).
pub const DEFAULT_QUIET_MS: u64 = 500;
/// Default bound on simultaneously-pending paths in the coalescer.
pub const DEFAULT_COALESCE_CAPACITY: usize = 8192;
/// Bound of the raw watcher→pump channel (normalized batches).
const RAW_EVENT_QUEUE_BOUND: usize = 4096;
/// Default scheduler saturation threshold used by the service step.
const DEFAULT_TASK_MAX_PENDING: usize = 4096;
/// Pump tick: how often pending coalesced state is checked for flush.
const PUMP_TICK: Duration = Duration::from_millis(100);
/// Fallback reconciliation interval when no native watcher is available.
pub const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
/// Wall-clock budget for one fallback reconciliation pass (bounded work).
const FALLBACK_PASS_BUDGET: Duration = Duration::from_secs(60);

/// How incremental change detection is currently running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// Native OS watcher active.
    NativeWatcher,
    /// Native watcher unavailable: bounded periodic authoritative
    /// reconciliation is running instead (a REAL mechanism, not a label).
    PeriodicReconciliation,
}

impl WatchMode {
    /// Stable string for MCP status exposure.
    pub fn as_str(&self) -> &'static str {
        match self {
            WatchMode::NativeWatcher => "native-watcher",
            WatchMode::PeriodicReconciliation => "periodic-reconciliation",
        }
    }
}

/// Shared counters / flags for observability + MCP status.
#[derive(Debug, Default)]
pub struct ServiceMetrics {
    /// Raw normalized events accepted.
    pub events_ingested: AtomicU64,
    /// Hints shed because the coalescer was full.
    pub hints_dropped: AtomicU64,
    /// Watcher error batches received (event-loss signal).
    pub watcher_errors: AtomicU64,
    /// Batches dropped by the raw watcher channel (saturation).
    pub raw_batches_dropped: AtomicU64,
    /// Verified change sets applied.
    pub changesets_applied: AtomicU64,
    /// Whether event loss requires authoritative reconciliation.
    pub reconciliation_required: AtomicBool,
}

/// Wall-clock milliseconds for the event timeline (coalescer unit = ms).
pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bounded, saturation-aware watcher pipe
// ---------------------------------------------------------------------------

/// Bounded non-blocking pipe between the notify callback thread and the pump.
///
/// `send` NEVER blocks and NEVER silently loses correctness: a full or
/// disconnected pipe records the drop and flips `dropped`, which the pump
/// converts into reconciliation-required semantics.
pub(crate) struct RawPipe<T> {
    tx: std::sync::mpsc::SyncSender<T>,
    pub dropped: Arc<AtomicU64>,
}

impl<T> RawPipe<T> {
    pub fn channel(capacity: usize) -> (Self, std::sync::mpsc::Receiver<T>, Arc<AtomicU64>) {
        let (tx, rx) = std::sync::mpsc::sync_channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                tx,
                dropped: Arc::clone(&dropped),
            },
            rx,
            dropped,
        )
    }

    /// Try to enqueue; on saturation record the drop (never block).
    pub fn send(&self, msg: T) -> bool {
        use std::sync::mpsc::TrySendError;
        match self.tx.try_send(msg) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.dropped.fetch_add(1, Ordering::SeqCst);
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Service core
// ---------------------------------------------------------------------------

/// The incremental service core.
///
/// Owns the bounded coalescer + policy-derived event filter; DB access flows
/// through the Phase 1A pool + coordinated writer exactly like every other
/// subsystem.
pub struct IncrementalService {
    root: PathBuf,
    policy: DiscoveryPolicy,
    filter: EventFilter,
    repo_id: Mutex<Option<String>>,
    coalescer: Arc<Mutex<EventCoalescer>>,
    pub(crate) metrics: Arc<ServiceMetrics>,
    quiet_ms: u64,
}

impl IncrementalService {
    /// Create a service for one repository root under the given policy.
    pub fn new(root: &Path, policy: DiscoveryPolicy) -> Self {
        let filter = EventFilter::new(policy.clone());
        Self {
            root: root.to_path_buf(),
            filter,
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

    /// Override the debounce quiet period in milliseconds (tests use tiny
    /// values).
    pub fn with_quiet_period_ms(mut self, ms: u64) -> Self {
        self.quiet_ms = ms;
        self.coalescer = Arc::new(Mutex::new(EventCoalescer::new(
            ms,
            DEFAULT_COALESCE_CAPACITY,
        )));
        self
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
    /// Security-blocked events are always discarded.  Policy-ineligible paths
    /// are discarded too (Phase 1B parity).  Returns `false` if any accepted
    /// path had to be shed because the coalescer was full — callers MUST
    /// treat state as uncertain until reconciliation completes.
    pub fn ingest(&self, evs: &[events::NormalizedEvent]) -> bool {
        let now_ms = now_millis();
        let mut all_accepted = true;
        let mut c_guard = match self.coalescer.lock() {
            Ok(c) => c,
            Err(_) => {
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
                return false;
            }
        };
        for ev in evs {
            if EventFilter::is_security_blocked(&ev.rel_path) {
                continue;
            }
            if !self.filter.is_eligible(&ev.rel_path) {
                continue;
            }
            self.metrics.events_ingested.fetch_add(1, Ordering::Relaxed);
            if !c_guard.push(ev, now_ms) {
                all_accepted = false;
                self.metrics.hints_dropped.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
            }
        }
        if c_guard.overflowed() {
            self.metrics
                .reconciliation_required
                .store(true, Ordering::SeqCst);
        }
        all_accepted
    }

    /// Drain coalesced hints whose quiet period elapsed at `now_override_ms`
    /// (`None` = wall clock, in milliseconds).  Pure — no I/O.
    pub fn drain_due(&self, now_override_ms: Option<u64>) -> Vec<CoalescedChange> {
        let now_ms = now_override_ms.unwrap_or_else(now_millis);
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
    /// Deterministic tests drive this with explicit timestamps; the watch
    /// pump calls it every tick so a single event flushes once the quiet
    /// period elapses even if no further batch ever arrives.
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

    /// Apply an explicit operation list (deterministic entry point).
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
        self.apply_verified_change_set(pool, writer, &cs)
    }

    /// Apply an ALREADY-VERIFIED change set directly.
    ///
    /// Used by reconciliation flows whose facts come from the authoritative
    /// discovery walk (e.g. discovery-policy exclusions where the file still
    /// exists on disk); per-hint disk re-verification would wrongly cancel
    /// them, so it is skipped by construction here.
    ///
    /// Uncertain paths (read/hash failures) NEVER flow into deletion or
    /// recompute: they are marked UNKNOWN and demand reconciliation.
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

        // ── Uncertain paths: degrade trust, never delete/recompute blind ──
        if !cs.uncertain.is_empty() {
            report.uncertain_paths = cs.uncertain.len();
            self.metrics
                .reconciliation_required
                .store(true, Ordering::SeqCst);
            let repo_id = self.resolve_repo(pool)?;
            mark_paths_unknown(writer, &repo_id, &cs.uncertain)?;
            recovery::schedule_reconciliation(writer)?;
        }

        // ── Verified restorations: hash matched ⇒ UNKNOWN→CURRENT ─────────
        if !cs.restored.is_empty() {
            let repo_id = self.resolve_repo(pool)?;
            apply_restored(writer, &repo_id, &cs.restored)?;
            report.restored_paths = cs.restored.len();
        }

        if cs.policy_changed {
            debug!("discovery-policy input changed; scheduling targeted rediscovery");
            recovery::schedule_reconciliation(writer)?;
            report.policy_rediscovery_scheduled = true;
        }

        if !cs.has_verified_work() {
            return Ok(report);
        }

        let repo_id = self.resolve_repo(pool)?;
        let outcome = invalidate_and_schedule(writer, &repo_id, cs, DEFAULT_TASK_MAX_PENDING)?;
        match outcome {
            ScheduleOutcome::Queued => report.task_queued = true,
            ScheduleOutcome::Deduplicated => report.task_deduplicated = true,
            ScheduleOutcome::Saturated => {
                self.metrics
                    .reconciliation_required
                    .store(true, Ordering::SeqCst);
                mark_paths_unknown(
                    writer,
                    &repo_id,
                    &cs.touched_paths().into_iter().collect::<Vec<_>>(),
                )?;
                recovery::schedule_reconciliation(writer)?;
                report.queue_saturated = true;
            }
        }
        self.metrics
            .changesets_applied
            .fetch_add(1, Ordering::Relaxed);
        Ok(report)
    }

    /// React to watcher errors / event loss (possible missed changes):
    /// require authoritative reconciliation.
    pub fn on_watcher_error(&self, detail: &str) {
        self.metrics.watcher_errors.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .reconciliation_required
            .store(true, Ordering::SeqCst);
        warn!(detail, "watcher error — reconciliation required");
    }

    /// Observe raw-pipe drops recorded by the notify callback thread.
    pub fn note_raw_drops(&self, dropped: &Arc<AtomicU64>) {
        let n = dropped.swap(0, Ordering::SeqCst);
        if n > 0 {
            self.metrics
                .raw_batches_dropped
                .fetch_add(n, Ordering::Relaxed);
            self.on_watcher_error("raw watcher queue saturated; batches dropped");
        }
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

    /// Aggregate status snapshot for the MCP `status` tool.
    pub fn status_snapshot(&self, pool: &DbPool) -> Result<ServiceStatus, IncrementalError> {
        let freshness = pool.with_reader(attic_storage::get_freshness_totals)?;
        let tasks = scheduler::queue_status(pool)?;
        Ok(ServiceStatus {
            events_ingested: self.metrics.events_ingested.load(Ordering::Relaxed),
            hints_dropped: self.metrics.hints_dropped.load(Ordering::Relaxed),
            watcher_errors: self.metrics.watcher_errors.load(Ordering::Relaxed),
            raw_batches_dropped: self.metrics.raw_batches_dropped.load(Ordering::Relaxed),
            reconciliation_required: self.reconciliation_required(),
            freshness,
            tasks,
        })
    }

    // -----------------------------------------------------------------------
    // Server-mode wiring
    // -----------------------------------------------------------------------

    /// Start change detection for server mode.
    ///
    /// Tries the native OS watcher first; if it cannot start, starts a REAL
    /// bounded periodic authoritative reconciliation loop instead and reports
    /// [`WatchMode::PeriodicReconciliation`].  There is no fake degraded
    /// label: both modes perform actual work.
    pub fn start_incremental_watch(
        self: &Arc<Self>,
        pool: DbPool,
        writer: WriterQueueHandle,
    ) -> Result<IncrementalWatch, IncrementalError> {
        match self.spawn_watcher(Arc::clone(self), pool.clone(), writer.clone()) {
            Ok(guard) => Ok(IncrementalWatch::Native(guard)),
            Err(e) => {
                warn!(
                    error = %e,
                    "native watcher unavailable — starting bounded periodic \
                     authoritative reconciliation fallback"
                );
                let guard = FallbackGuard::spawn(
                    Arc::clone(self),
                    pool,
                    writer,
                    FALLBACK_RECONCILE_INTERVAL,
                )?;
                Ok(IncrementalWatch::Periodic(guard))
            }
        }
    }

    /// Native watcher + bounded pump.
    fn spawn_watcher(
        self: &Arc<Self>,
        svc: Arc<Self>,
        pool: DbPool,
        writer: WriterQueueHandle,
    ) -> Result<WatcherGuard<notify_debouncer_full::RecommendedCache>, IncrementalError> {
        use std::sync::mpsc::Receiver;
        use std::time::Instant;

        enum PumpMsg {
            Batch(notify_debouncer_full::DebounceEventResult),
        }

        // Bounded + non-blocking: saturation records a drop instead of
        // blocking the notify callback thread.
        let (pipe, rx, dropped): (RawPipe<PumpMsg>, Receiver<PumpMsg>, Arc<AtomicU64>) =
            RawPipe::channel(RAW_EVENT_QUEUE_BOUND);

        let quiet = Duration::from_millis(self.quiet_ms);
        let mut debouncer = notify_debouncer_full::new_debouncer(
            quiet,
            None,
            move |res: notify_debouncer_full::DebounceEventResult| {
                // Never blocks; full pipe ⇒ recorded drop.
                let _ = pipe.send(PumpMsg::Batch(res));
            },
        )
        .map_err(|e| IncrementalError::Io(std::io::Error::other(e.to_string())))?;

        debouncer
            .watch(
                self.root.clone(),
                notify_debouncer_full::notify::RecursiveMode::Recursive,
            )
            .map_err(|e| IncrementalError::Io(std::io::Error::other(e.to_string())))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_pump = Arc::clone(&stop);
        let svc_pump = Arc::clone(&svc);
        let handle = std::thread::Builder::new()
            .name("attic-watch-pump".into())
            .spawn(move || {
                // Deadline-bounded loop; ticks even without batch arrival so
                // the quiet-period flush ALWAYS happens.
                let deadline = Instant::now() + Duration::from_secs(300);
                loop {
                    if stop_pump.load(Ordering::SeqCst) || Instant::now() >= deadline {
                        break;
                    }
                    match rx.recv_timeout(PUMP_TICK) {
                        Ok(PumpMsg::Batch(Ok(debounced))) => {
                            let mut normalized = Vec::new();
                            for ev in &debounced {
                                normalized.extend(events::normalize_debounced(ev, &svc_pump.root));
                            }
                            svc_pump.note_raw_drops(&dropped);
                            if !normalized.is_empty() {
                                svc_pump.ingest(&normalized);
                            }
                            // Flush whatever the quiet period has released.
                            if let Err(e) = svc_pump.apply_pending(&pool, &writer, None) {
                                warn!(error = %e, "watch pump apply failed");
                            }
                        }
                        Ok(PumpMsg::Batch(Err(errors))) => {
                            svc_pump.note_raw_drops(&dropped);
                            for e in errors {
                                svc_pump.on_watcher_error(&e.to_string());
                            }
                            let _ = recovery::schedule_reconciliation(&writer);
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // TICK: pending coalesced events whose quiet
                            // period elapsed are flushed here even when the
                            // watcher produced nothing further.
                            svc_pump.note_raw_drops(&dropped);
                            if let Err(e) = svc_pump.apply_pending(&pool, &writer, None) {
                                warn!(error = %e, "watch pump tick apply failed");
                            }
                        }
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
}

/// Shared invalidation → scheduling step used by BOTH the service and the
/// scheduler's RECONCILIATION arm.  Invalidation stays synchronous/cheap;
/// recomputation is only SCHEDULED here (never executed inline), preserving
/// `invalidation != recomputation`.
pub(crate) fn invalidate_and_schedule(
    writer: &WriterQueueHandle,
    repo_id: &str,
    cs: &VerifiedChangeSet,
    max_pending: usize,
) -> Result<ScheduleOutcome, IncrementalError> {
    let counts = invalidation::apply_invalidation(
        writer,
        repo_id,
        cs,
        attic_core::InvalidationCause::SourceChanged,
        crate::now_micros(),
    )?;
    debug!(
        occurrences = counts.occurrences_marked,
        derived = counts.derived_invalidated,
        "invalidation applied"
    );

    let payload = IncrementalTaskPayload {
        dedup_key: dedup_key(cs),
        upserts: cs.upserts.clone(),
        deletes: cs.deletes.clone(),
        renames: cs.renames.clone(),
        from_reconciliation: false,
    };
    let priority = if payload.from_reconciliation {
        scheduler::PRIORITY_RECONCILE
    } else {
        scheduler::PRIORITY_USER_EDIT
    };
    scheduler::schedule_incremental(writer, repo_id, &payload, priority, max_pending)
}

/// Verified restoration: disk hash matched the stored hash, so trust is
/// re-established WITHOUT any recomputation (UNKNOWN/STALE → CURRENT is a
/// legal verified transition; audit records are closed).
pub(crate) fn apply_restored(
    writer: &WriterQueueHandle,
    repo_id: &str,
    paths: &[String],
) -> Result<(), IncrementalError> {
    let typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| IncrementalError::NotBootstrapped(repo_id.to_owned()))?;
    let paths = paths.to_vec();
    run_on_writer(writer, move |conn| {
        for p in &paths {
            if let Some(snap) = attic_storage::lookup_occurrence_snapshot(conn, &typed, p)? {
                conn.execute(
                    "UPDATE core_file_occurrences
                        SET freshness_state = 'CURRENT'
                      WHERE id = ?1
                        AND freshness_state IN ('UNKNOWN', 'STALE', 'PENDING_REFRESH')
                        AND existence_state != 'deleted'",
                    [&snap.id],
                )?;
                attic_storage::record_recomputed(conn, &snap.id, crate::now_micros())?;
            }
        }
        Ok(())
    })
}

/// Mark specific paths' latest occurrences UNKNOWN (trust degradation).
fn mark_paths_unknown(
    writer: &WriterQueueHandle,
    repo_id: &str,
    paths: &[String],
) -> Result<(), IncrementalError> {
    let typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| IncrementalError::NotBootstrapped(repo_id.to_owned()))?;
    let paths: Vec<String> = paths.to_vec();
    run_on_writer(writer, move |conn| {
        for p in &paths {
            if let Some(snap) = attic_storage::lookup_occurrence_snapshot(conn, &typed, p)? {
                conn.execute(
                    "UPDATE core_file_occurrences SET freshness_state = 'UNKNOWN'
                      WHERE id = ?1 AND freshness_state IN ('CURRENT','STALE')",
                    [&snap.id],
                )?;
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Watch handles
// ---------------------------------------------------------------------------

/// Handle over whichever change-detection mechanism is running.
pub enum IncrementalWatch {
    /// Native OS watcher + bounded pump.
    Native(WatcherGuard<notify_debouncer_full::RecommendedCache>),
    /// Bounded periodic authoritative reconciliation loop.
    Periodic(FallbackGuard),
}

impl IncrementalWatch {
    /// Which mechanism is running.
    pub fn mode(&self) -> WatchMode {
        match self {
            IncrementalWatch::Native(_) => WatchMode::NativeWatcher,
            IncrementalWatch::Periodic(_) => WatchMode::PeriodicReconciliation,
        }
    }

    /// Stop the background loop(s).  Idempotent per variant Drop.
    pub fn stop(&mut self) {
        match self {
            IncrementalWatch::Native(g) => g.signal_stop_and_join(),
            IncrementalWatch::Periodic(g) => g.stop(),
        }
    }
}

/// Bounded periodic authoritative reconciliation loop (real fallback work:
/// reconcile → verified change set → invalidation → scheduled recomputation).
pub struct FallbackGuard {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl FallbackGuard {
    fn spawn(
        svc: Arc<IncrementalService>,
        pool: DbPool,
        writer: WriterQueueHandle,
        interval: Duration,
    ) -> Result<Self, IncrementalError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let root = svc.root.clone();
        let policy = svc.policy().clone();
        let worker = std::thread::Builder::new()
            .name("attic-fallback-reconcile".into())
            .spawn(move || {
                loop {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let pass_start = std::time::Instant::now();
                    match recovery::reconcile_repository(&pool, &writer, &root, &policy) {
                        Ok(report) => {
                            if !report.change_set.uncertain.is_empty() {
                                svc.on_watcher_error("reconciliation found unreadable paths");
                            }
                            if report.change_set.has_verified_work()
                                || !report.change_set.uncertain.is_empty()
                            {
                                if let Err(e) = svc.apply_verified_change_set(
                                    &pool,
                                    &writer,
                                    &report.change_set,
                                ) {
                                    warn!(error = %e, "fallback reconciliation apply failed");
                                }
                            } else {
                                svc.clear_reconciliation_flag();
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "fallback reconciliation failed");
                        }
                    }
                    // Bound each pass even if individual steps stall behind
                    // their own internal deadlines.
                    let spent = pass_start.elapsed();
                    if spent >= FALLBACK_PASS_BUDGET.min(interval) {
                        continue;
                    }
                    // Interruptible sleep.
                    let sleep_for = interval.saturating_sub(spent);
                    let wake = std::time::Instant::now() + sleep_for;
                    while std::time::Instant::now() < wake {
                        if stop_flag.load(Ordering::SeqCst) {
                            return;
                        }
                        let remaining = wake.saturating_duration_since(std::time::Instant::now());
                        std::thread::sleep(Duration::from_millis(50).min(remaining));
                    }
                }
            })
            .map_err(|e| IncrementalError::Scheduler(format!("fallback reconcile spawn: {e}")))?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    /// Stop the loop and join its thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FallbackGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Stop handle owning the native watcher + pump thread.
pub struct WatcherGuard<C: notify_debouncer_full::FileIdCache> {
    _debouncer: Mutex<
        Option<
            notify_debouncer_full::Debouncer<notify_debouncer_full::notify::RecommendedWatcher, C>,
        >,
    >,
    stop: Arc<AtomicBool>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl<C: notify_debouncer_full::FileIdCache> WatcherGuard<C> {
    /// Stop pump + watcher and join threads.
    pub fn signal_stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut g) = self._debouncer.lock() {
            *g = None; // dropping the debouncer stops its threads
        }
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

impl<C: notify_debouncer_full::FileIdCache> Drop for WatcherGuard<C> {
    fn drop(&mut self) {
        self.signal_stop_and_join();
    }
}

// ---------------------------------------------------------------------------
// Reports / status
// ---------------------------------------------------------------------------

/// One pipeline-step summary (observability + tests).
#[derive(Debug, Default, Clone)]
pub struct StepReport {
    /// Verified upsert count.
    pub verified_upserts: usize,
    /// Verified delete count.
    pub verified_deletes: usize,
    /// Verified rename pair count.
    pub verified_renames: usize,
    /// Paths that could not be read/verified (degraded to UNKNOWN).
    pub uncertain_paths: usize,
    /// Paths verified back to CURRENT without recomputation.
    pub restored_paths: usize,
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
    /// Batches dropped by the bounded raw channel (saturation).
    pub raw_batches_dropped: u64,
    /// Whether authoritative reconciliation is required.
    pub reconciliation_required: bool,
    /// Occurrence freshness totals.
    pub freshness: attic_storage::FreshnessTotals,
    /// Task queue counters.
    pub tasks: scheduler::QueueStatus,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn raw_pipe_never_blocks_and_records_saturation() {
        let (pipe, rx, dropped) = RawPipe::<u32>::channel(2);
        assert!(pipe.send(1));
        assert!(pipe.send(2));
        // Third send must NOT block and MUST be recorded as a drop.
        let t0 = Instant::now();
        let accepted = pipe.send(3);
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "send must never block"
        );
        assert!(!accepted, "saturated send reports failure");
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        drop(rx); // disconnect
        assert!(!pipe.send(4), "disconnected pipe reports failure");
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
}
