//! Phase 2 task scheduler — bounded queues over durable `ops_tasks` rows.
//!
//! Scope (Phase 2 only — NOT the Phase 7 adaptive scheduler):
//! - bounded pending depth (`max_pending`);
//! - priorities from the schema (`priority DESC, created_at ASC`);
//! - idempotent enqueue (identical payload dedup, ADR-009);
//! - cancellation of still-PENDING tasks + graceful shutdown that leaves
//!   RUNNING tasks recoverable (they return to PENDING at next startup);
//! - retry via `ops_tasks.retry_count/max_retries`.
//!
//! Duplicate watcher events cannot produce duplicate canonical mutations:
//! dedup happens at enqueue AND publication is atomic per run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, warn};

use attic_indexing::IndexOptions;
use attic_storage::{
    ClaimedTask, DbPool, EnqueueOutcome, IncrementalTaskPayload, TASK_INCREMENTAL_INDEX,
    TASK_RECONCILIATION, TaskCounts, TaskOutcome, WriterQueueHandle, claim_next_pending_task,
    enqueue_task, finish_task, get_task_counts, set_task_checkpoint,
};

use crate::changeset::VerifiedChangeSet;
use crate::{IncrementalError, run_on_writer};

/// Priority for user-visible edits (highest).
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
pub fn schedule_incremental(
    writer: &WriterQueueHandle,
    repo_id: &str,
    payload: &IncrementalTaskPayload,
    priority: i64,
    max_pending: usize,
) -> Result<ScheduleOutcome, IncrementalError> {
    let counts: TaskCounts = run_on_writer(writer, get_task_counts)?;
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
/// The store pair is cloned into every worker; all writes still serialize
/// through the single coordinated writer queue.
pub fn spawn_scheduler(
    config: SchedulerConfig,
    pool: DbPool,
    writer: WriterQueueHandle,
    root: std::path::PathBuf,
    policy: attic_discovery::DiscoveryPolicy,
) -> SchedulerHandle {
    let shutdown = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ShutdownState::default());
    let mut workers = Vec::with_capacity(config.workers.max(1));

    for worker_idx in 0..config.workers.max(1) {
        let cfg = config.clone();
        let pool = pool.clone();
        let writer = writer.clone();
        let root = root.clone();
        let policy = policy.clone();
        let shutdown = Arc::clone(&shutdown);
        let st = Arc::clone(&state);
        match std::thread::Builder::new()
            .name(format!("attic-sched-{worker_idx}"))
            .spawn(move || {
                worker_loop(cfg, pool, writer, root, policy, shutdown, st);
            }) {
            Ok(h) => workers.push(h),
            Err(e) => {
                warn!(error = %e, "scheduler worker spawn failed");
                break;
            }
        }
    }

    SchedulerHandle {
        shutdown,
        state,
        workers,
    }
}

fn wait_for_wake_or_timeout(state: &ShutdownState, timeout: Duration) {
    let Ok(g) = state.stop_accepting.lock() else {
        std::thread::sleep(timeout);
        return;
    };
    let _ = state.cv.wait_timeout_while(g, timeout, |stopped| !*stopped);
}

fn worker_loop(
    config: SchedulerConfig,
    pool: DbPool,
    writer: WriterQueueHandle,
    root: std::path::PathBuf,
    policy: attic_discovery::DiscoveryPolicy,
    shutdown: Arc<AtomicBool>,
    state: Arc<ShutdownState>,
) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            debug!("scheduler worker exiting (graceful)");
            return;
        }

        // Claim atomically through the coordinated writer queue.
        let claimed = run_on_writer(&writer, |conn| {
            claim_next_pending_task(conn, crate::now_micros())
        });
        match claimed {
            Ok(Some(task)) => {
                debug!(task = %task.id, kind = %task.task_type, "executing task");
                let outcome = execute_task(&pool, &writer, &root, &policy, &task);
                let task_id = task.id.clone();
                let finished: Result<(), IncrementalError> = run_on_writer(&writer, move |conn| {
                    finish_task(conn, &task_id, &outcome, crate::now_micros())
                });
                if let Err(e) = finished {
                    warn!(task = %task.id, error = %e, "finish_task failed");
                }
            }
            Ok(None) => {
                if shutdown.load(Ordering::SeqCst) {
                    continue;
                }
                wait_for_wake_or_timeout(&state, config.poll_interval);
            }
            Err(e) => {
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
    task: &ClaimedTask,
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
            match crate::recovery::reconcile_repository(pool, writer, root, policy) {
                Ok(report) => {
                    debug!(
                        changed = report.changed_paths,
                        excluded = report.newly_excluded,
                        "reconciliation done"
                    );
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
) -> Result<bool, IncrementalError> {
    let claimed = run_on_writer(writer, |conn| {
        claim_next_pending_task(conn, crate::now_micros())
    })?;
    let Some(task) = claimed else {
        return Ok(false);
    };
    debug!(task = %task.id, kind = %task.task_type, "sync-executing task");
    let outcome = execute_task(pool, writer, root, policy, &task);
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
