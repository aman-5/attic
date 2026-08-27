//! Phase 2 task scheduler — bounded queues over durable `ops_tasks` rows.
//! Phase 7 addition: resource-pressure aware scheduling that ensures foreground
//! user work is never starved by background indexing/enrichment.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, warn};

use attic_indexing::IndexOptions;
use attic_storage::{
    ClaimedTask, DbPool, EnqueueOutcome, IncrementalTaskPayload, ResourceMonitor,
    TASK_INCREMENTAL_INDEX, TASK_RECONCILIATION, TaskCounts, TaskOutcome, WriterQueueHandle,
    claim_next_pending_task, enqueue_task, finish_task, get_task_counts, set_task_checkpoint,
};

use crate::changeset::VerifiedChangeSet;
use crate::{IncrementalError, run_on_writer};

/// Origin of a recomputation request — decides queue priority and the
/// `from_reconciliation` flag so reconciliation-generated work is never
/// mistaken for ordinary user-edit work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOrigin {
    /// Direct watcher/user-driven change.
    UserEdit,
    /// Produced by an authoritative reconciliation pass.
    Reconciliation,
}

impl TaskOrigin {
    /// Queue priority for this origin.
    pub fn priority(self) -> i64 {
        match self {
            TaskOrigin::UserEdit => PRIORITY_USER_EDIT,
            TaskOrigin::Reconciliation => PRIORITY_RECONCILE,
        }
    }

    /// Payload flag value for this origin.
    pub fn from_reconciliation(self) -> bool {
        matches!(self, TaskOrigin::Reconciliation)
    }
}

pub const PRIORITY_USER_EDIT: i64 = 80;
/// Priority for reconciliation refreshes.
pub const PRIORITY_RECONCILE: i64 = 40;

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Worker threads.  Bounded recomputation concurrency.
    pub workers: usize,
    /// Maximum PENDING tasks before enqueue sheds (caller must reconcile).
    pub max_pending: usize,
    /// Idle poll interval.
    pub poll_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            max_pending: 4096,
            poll_interval: Duration::from_millis(200),
        }
    }
}

impl SchedulerConfig {
    /// Validate configuration before any thread is created.
    pub fn validate(&self) -> Result<(), IncrementalError> {
        if self.workers == 0 {
            return Err(IncrementalError::Scheduler(
                "workers must be >= 1 (a zero-worker scheduler would never execute tasks)".into(),
            ));
        }
        if self.max_pending == 0 {
            return Err(IncrementalError::Scheduler(
                "max_pending must be >= 1 (zero would shed every enqueue)".into(),
            ));
        }
        Ok(())
    }
}

/// Result of an idempotent enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleOutcome {
    /// Task created.
    Queued,
    /// Identical task already pending.
    Deduplicated,
    /// Pending queue at capacity — caller MUST mark state UNKNOWN and
    /// schedule reconciliation; silent loss is forbidden by contract §13.
    Saturated,
}

/// Build a deterministic dedup key for a change set (order-independent).
pub fn dedup_key(cs: &VerifiedChangeSet) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.extend(cs.upserts.iter().map(|p| format!("u:{p}")));
    parts.extend(cs.deletes.iter().map(|p| format!("d:{p}")));
    parts.extend(cs.renames.iter().map(|(f, t)| format!("r:{f}>{t}")));
    parts.sort();
    blake3::hash(parts.join("|").as_bytes())
        .to_hex()
        .to_string()
}
/// Enqueue one incremental recompute task (idempotent + bounded).
///
/// Phase 7 addition: checks the global resource monitor before enqueuing
/// background tasks.  When memory pressure is critical or emergency, only
/// UserEdit (foreground) tasks are accepted; reconciliation and other
/// background work is deferred to prevent starving foreground MCP queries.
pub fn schedule_incremental(
    writer: &WriterQueueHandle,
    repo_id: &str,
    payload: &IncrementalTaskPayload,
    priority: i64,
    max_pending: usize,
    monitor: Option<&ResourceMonitor>,
) -> Result<ScheduleOutcome, IncrementalError> {
    let counts: TaskCounts = run_on_writer(writer, get_task_counts)?;

    // Phase 7: resource-pressure gate — under critical/Emergency pressure,
    // only accept foreground (UserEdit) priority tasks; defer background work.
    if let Some(mon) = monitor {
        let pressure = mon.pressure();
        // Emergency: only accept priority >= 70 (roughly UserEdit range).
        // Critical: only accept priority >= 60.
        // Warning: accept all but log.
        let only_foreground = matches!(
            pressure,
            attic_core::domain::enums::ResourcePressure::Emergency
                | attic_core::domain::enums::ResourcePressure::Critical
        );

        if only_foreground && priority < 70 {
            debug!(
                "resource pressure {:?} deferring background task priority={}",
                pressure, priority
            );
            return Ok(ScheduleOutcome::Saturated); // defer — caller should reconcile
        }
    }

    if counts.pending >= max_pending as i64 {
        return Ok(ScheduleOutcome::Saturated);
    }

    let json = serde_json::to_string(payload).map_err(attic_storage::StorageError::from)?;
    let id = uuid::Uuid::new_v4().to_string();
    let repo_owned = repo_id.to_owned();
    let outcome: EnqueueOutcome = run_on_writer(writer, move |conn| {
        enqueue_task(
            conn,
            &id,
            Some(&repo_owned),
            TASK_INCREMENTAL_INDEX,
            priority,
            &json,
            crate::now_micros(),
        )
    })?;
    match outcome {
        EnqueueOutcome::Created => Ok(ScheduleOutcome::Queued),
        EnqueueOutcome::AlreadyPending => Ok(ScheduleOutcome::Deduplicated),
    }
}

// ---------------------------------------------------------------------------
// Worker pool
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ShutdownState {
    stop_accepting: Mutex<bool>,
    cv: Condvar,
}

/// Handle to a spawned scheduler.
pub struct SchedulerHandle {
    shutdown: Arc<AtomicBool>,
    state: Arc<ShutdownState>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl SchedulerHandle {
    /// Signal graceful shutdown and join all workers.
    ///
    /// In-flight tasks are allowed to finish; unclaimed PENDING rows stay in
    /// `ops_tasks` and resume after restart.  RUNNING tasks interrupted by a
    /// hard kill return to PENDING via startup recovery.
    pub fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Ok(mut g) = self.state.stop_accepting.lock() {
            *g = true;
        }
        self.state.cv.notify_all();
        for h in self.workers {
            let _ = h.join();
        }
    }

    /// Non-blocking variant used on Drop paths (best-effort).
    pub fn signal_stop(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.state.cv.notify_all();
    }
}

impl std::fmt::Debug for SchedulerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SchedulerHandle").finish()
    }
}

/// Spawn `config.workers` worker threads.
///
/// Fallible by contract: configuration is validated first, and if ANY worker
/// thread fails to spawn the already-started workers are shut down and an
/// error is returned — a handle that would never execute tasks is never
/// handed out.
pub fn spawn_scheduler(
    config: SchedulerConfig,
    pool: DbPool,
    writer: WriterQueueHandle,
    root: std::path::PathBuf,
    policy: attic_discovery::DiscoveryPolicy,
    monitor: Option<Arc<ResourceMonitor>>,
) -> Result<SchedulerHandle, IncrementalError> {
    config.validate()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ShutdownState::default());
    let mut workers = Vec::with_capacity(config.workers);

    for worker_idx in 0..config.workers {
        let cfg = config.clone();
        let pool = pool.clone();
        let writer = writer.clone();
        let root = root.clone();
        let policy = policy.clone();
        let worker_shutdown = Arc::clone(&shutdown);
        let st = Arc::clone(&state);
        let monitor_captured = monitor.clone();
        match std::thread::Builder::new()
            .name(format!("attic-sched-{worker_idx}"))
            .spawn(move || {
                worker_loop(
                    cfg,
                    pool,
                    writer,
                    root,
                    policy,
                    worker_shutdown,
                    st,
                    monitor_captured,
                );
            }) {
            Ok(h) => workers.push(h),
            Err(e) => {
                // Partial startup: stop what did start, then fail loudly.
                shutdown.store(true, Ordering::SeqCst);
                if let Ok(mut g) = state.stop_accepting.lock() {
                    *g = true;
                }
                state.cv.notify_all();
                for h in workers.drain(..) {
                    let _ = h.join();
                }
                return Err(IncrementalError::Scheduler(format!(
                    "worker thread {} failed to spawn: {e}",
                    workers.len()
                )));
            }
        }
    }

    debug!(workers = workers.len(), "scheduler started");
    Ok(SchedulerHandle {
        shutdown,
        state,
        workers,
    })
}

fn wait_for_wake_or_timeout(state: &ShutdownState, timeout: Duration) {
    let Ok(g) = state.stop_accepting.lock() else {
        std::thread::sleep(timeout);
        return;
    };
    let _ = state.cv.wait_timeout_while(g, timeout, |stopped| !*stopped);
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    config: SchedulerConfig,
    pool: DbPool,
    writer: WriterQueueHandle,
    root: std::path::PathBuf,
    policy: attic_discovery::DiscoveryPolicy,
    shutdown: Arc<AtomicBool>,
    state: Arc<ShutdownState>,
    monitor: Option<Arc<ResourceMonitor>>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            debug!("scheduler worker exiting (graceful)");
            return;
        }

        // Refresh real process memory before admission decisions so the
        // pressure gate and background slot limits reflect genuine RSS.
        if let Some(m) = monitor.as_ref() {
            m.refresh_process_memory();
            // Phase 7: background slot admission — a worker may not claim a
            // task without a background CPU slot.  Under Pause/Emergency
            // advisories the slot is refused, so the worker idles instead of
            // starved-by-design foreground queries.
            if !m.acquire_background_slot() {
                wait_for_wake_or_timeout(&state, config.poll_interval);
                continue;
            }
        }

        // Claim atomically through the coordinated writer queue.
        let claimed = run_on_writer(&writer, |conn| {
            claim_next_pending_task(conn, crate::now_micros())
        });
        match claimed {
            Ok(Some(task)) => {
                debug!(task = %task.id, kind = %task.task_type, "executing task");
                let outcome = execute_task(
                    &pool,
                    &writer,
                    &root,
                    &policy,
                    &config,
                    &task,
                    monitor.as_deref(),
                );
                let task_id = task.id.clone();
                let finished: Result<(), IncrementalError> = run_on_writer(&writer, move |conn| {
                    finish_task(conn, &task_id, &outcome, crate::now_micros())
                });
                if let Err(e) = finished {
                    warn!(task = %task.id, error = %e, "finish_task failed");
                }
                // Release the background slot only after the task fully finished.
                if let Some(m) = monitor.as_ref() {
                    m.release_background_slot();
                }
            }
            Ok(None) => {
                // No work claimed: return the background slot for this poll.
                if let Some(m) = monitor.as_ref() {
                    m.release_background_slot();
                }
                if shutdown.load(Ordering::SeqCst) {
                    continue;
                }
                wait_for_wake_or_timeout(&state, config.poll_interval);
            }
            Err(e) => {
                if let Some(m) = monitor.as_ref() {
                    m.release_background_slot();
                }
                warn!(error = %e, "task claim failed");
                std::thread::sleep(config.poll_interval);
            }
        }
    }
}

fn execute_task(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    root: &std::path::Path,
    policy: &attic_discovery::DiscoveryPolicy,
    config: &SchedulerConfig,
    task: &ClaimedTask,
    monitor: Option<&ResourceMonitor>,
) -> TaskOutcome {
    match task.task_type.as_str() {
        TASK_INCREMENTAL_INDEX => {
            let payload: Option<IncrementalTaskPayload> =
                serde_json::from_str(task.checkpoint_json.as_deref().unwrap_or("{}")).ok();
            let Some(payload) = payload else {
                return TaskOutcome::Failed {
                    error: "unreadable INCREMENTAL_INDEX payload".into(),
                };
            };

            // Progress checkpoint BEFORE work: recovery can report what was
            // in flight when a crash hit (recovery contract CP-04).
            let checkpoint = serde_json::json!({
                "dedup_key": payload.dedup_key,
                "phase": "executing",
                "upserts": payload.upserts.len(),
                "deletes": payload.deletes.len(),
            });
            let cp = checkpoint.to_string();
            let task_id = task.id.clone();
            let _: Result<(), IncrementalError> =
                run_on_writer(writer, move |conn| set_task_checkpoint(conn, &task_id, &cp));

            let cs = VerifiedChangeSet {
                upserts: payload.upserts.clone(),
                deletes: payload.deletes.clone(),
                renames: payload.renames.clone(),
                uncertain: vec![],
                restored: vec![],
                policy_changed: false,
            };
            // The scoped indexer takes its own verified-input shape.
            let scoped = attic_indexing::ScopedChanges {
                upserts: cs.upserts,
                deletes: cs.deletes,
                rename_hints: cs.renames,
            };
            let store = attic_indexing::IndexingStore {
                readers: pool,
                writer,
            };
            let opts = IndexOptions::default();
            match attic_indexing::index_changes(&store, root, policy, &opts, &scoped) {
                Ok(res) => {
                    debug!(
                        published = res.files_published,
                        deleted = res.files_deleted,
                        units = res.units_inserted,
                        "incremental task done"
                    );
                    TaskOutcome::Done
                }
                Err(e) => TaskOutcome::Failed {
                    error: e.to_string(),
                },
            }
        }
        TASK_RECONCILIATION => {
            // Authoritative diff — then take it through the SAME pipeline as
            // every other change: invalidation (cheap, sync) → schedule
            // INCREMENTAL_INDEX recomputation (separate task).  A converged
            // tree yields an empty change set and no follow-up work, so the
            // loop terminates.
            // Phase 7: if resource pressure is critical or emergency, skip
            // scheduling new incremental index tasks so foreground work is not
            // starved.  The diff itself is still performed (it's cheap and
            // non-blocking), but scheduling is deferred.
            let _should_defer_scheduling = monitor
                .map(|m| {
                    matches!(
                        m.pressure(),
                        attic_core::domain::enums::ResourcePressure::Emergency
                            | attic_core::domain::enums::ResourcePressure::Critical
                    )
                })
                .unwrap_or(false);

            match crate::recovery::reconcile_repository(pool, writer, root, policy) {
                Ok(report) => {
                    debug!(
                        changed = report.change_set.upserts.len() + report.change_set.deletes.len(),
                        uncertain = report.change_set.uncertain.len(),
                        "authoritative reconciliation diff complete"
                    );
                    let mut cs = report.change_set;
                    if let Ok(typed) = report.repository_id.parse::<attic_core::RepositoryId>()
                        && let Err(e) =
                            crate::service::pair_content_renames(root, pool, &typed, &mut cs)
                    {
                        return TaskOutcome::Failed {
                            error: format!("rename pairing failed: {e}"),
                        };
                    }
                    if !cs.uncertain.is_empty() {
                        let repo = report.repository_id.clone();
                        let paths = cs.uncertain.clone();
                        let _: Result<(), IncrementalError> =
                            crate::run_on_writer(writer, move |conn| {
                                if let Ok(typed) = repo.parse::<attic_core::RepositoryId>() {
                                    for p in &paths {
                                        if let Some(snap) =
                                            attic_storage::lookup_occurrence_snapshot(
                                                conn, &typed, p,
                                            )?
                                        {
                                            conn.execute(
                                                "UPDATE core_file_occurrences
                                                    SET freshness_state = 'UNKNOWN'
                                                  WHERE id = ?1
                                                  AND freshness_state IN ('CURRENT','STALE')",
                                                [&snap.id],
                                            )?;
                                        }
                                    }
                                }
                                Ok(())
                            });
                    }
                    if !cs.restored.is_empty()
                        && !report.repository_id.is_empty()
                        && let Err(e) = crate::service::apply_restored(
                            writer,
                            &report.repository_id,
                            &cs.restored,
                        )
                    {
                        return TaskOutcome::Failed {
                            error: format!("verified restore failed: {e}"),
                        };
                    }
                    if cs.has_verified_work() && !report.repository_id.is_empty() {
                        match crate::service::invalidate_and_schedule(
                            writer,
                            &report.repository_id,
                            &cs,
                            config.max_pending,
                            TaskOrigin::Reconciliation,
                            monitor, // pass monitor for pressure gate inside
                        ) {
                            Ok(outcome) => {
                                debug!(?outcome, "reconciliation scheduled recomputation");
                            }
                            Err(e) => {
                                return TaskOutcome::Failed {
                                    error: format!(
                                        "reconciliation invalidation/scheduling failed: {e}"
                                    ),
                                };
                            }
                        }
                    }
                    TaskOutcome::Done
                }
                Err(e) => TaskOutcome::Failed {
                    error: e.to_string(),
                },
            }
        }
        other => TaskOutcome::Failed {
            error: format!("unknown task type: {other}"),
        },
    }
}

/// Claim + execute + finish exactly one task synchronously.
///
/// Deterministic driver used by the test suite and available to embedders
/// that prefer their own loop over [`spawn_scheduler`] threads.
/// Returns `Ok(false)` when the queue was empty.
pub fn run_next_task_synchronously(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    root: &std::path::Path,
    policy: &attic_discovery::DiscoveryPolicy,
    monitor: Option<&ResourceMonitor>,
) -> Result<bool, IncrementalError> {
    let claimed = run_on_writer(writer, |conn| {
        claim_next_pending_task(conn, crate::now_micros())
    })?;
    let Some(task) = claimed else {
        return Ok(false);
    };
    debug!(task = %task.id, kind = %task.task_type, "sync-executing task");
    let outcome = execute_task(
        pool,
        writer,
        root,
        policy,
        &SchedulerConfig::default(),
        &task,
        monitor,
    );
    run_on_writer(writer, move |conn| {
        finish_task(conn, &task.id, &outcome, crate::now_micros())
    })?;
    Ok(true)
}

/// Serialize-friendly snapshot for MCP status.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct QueueStatus {
    /// PENDING task count.
    pub pending: i64,
    /// RUNNING task count.
    pub running: i64,
    /// FAILED task count.
    pub failed: i64,
}

/// Read current queue counts (status tool support).
pub fn queue_status(pool: &DbPool) -> Result<QueueStatus, IncrementalError> {
    let c = pool.with_reader(get_task_counts)?;
    Ok(QueueStatus {
        pending: c.pending,
        running: c.running,
        failed: c.failed,
    })
}
