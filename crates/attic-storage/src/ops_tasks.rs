//! Phase 2 — durable task queue over `ops_tasks`.
//!
//! All functions here are **transaction-assuming primitives**: they never open
//! their own transaction and are safe to call inside a writer-queue closure
//! (ambient `BEGIN IMMEDIATE … COMMIT`).  Task claiming is therefore atomic
//! with respect to every other writer-queue client.
//!
//! States used exactly as defined by `migrations/0001_initial.sql`:
//! `PENDING | RUNNING | DONE | FAILED | CANCELLED`.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::StorageError;

/// `ops_tasks.task_type` values used in Phase 2.
pub const TASK_INCREMENTAL_INDEX: &str = "INCREMENTAL_INDEX";
/// Authoritative bounded rescan task type.
pub const TASK_RECONCILIATION: &str = "RECONCILIATION";
/// Repository-removal data eviction. Claimed ONLY by the dedicated evictor
/// (`claim_next_pending_task_of_type`), never by the indexing scheduler.
/// `repository_id` is NULL (the row must not pin `core_repositories`); the
/// target repository id lives in `checkpoint_json` as `{"repository_id": ..}`.
pub const TASK_STALE_EVICTION: &str = "STALE_EVICTION";

/// Outcome of an idempotent enqueue attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// A new PENDING row was created with this id.
    Created,
    /// An identical PENDING row already existed; nothing was written.
    AlreadyPending,
}

/// A claimed (`RUNNING`) task row.
#[derive(Debug, Clone)]
pub struct ClaimedTask {
    /// `ops_tasks.id` (UUID string).
    pub id: String,
    /// Task type string (see [`TASK_INCREMENTAL_INDEX`]).
    pub task_type: String,
    /// Repository UUID string; `None` for workspace-scope tasks.
    pub repository_id: Option<String>,
    /// Priority (higher = more urgent), as stored.
    pub priority: i64,
    /// Opaque JSON payload / checkpoint state.  No secret content.
    pub checkpoint_json: Option<String>,
    /// Completed retry attempts so far.
    pub retry_count: i64,
    /// Maximum allowed retries.
    pub max_retries: i64,
}

/// Terminal-or-retry outcome reported by a worker.
#[derive(Debug, Clone)]
pub enum TaskOutcome {
    /// Task completed successfully.
    Done,
    /// Task failed; may be retried if `retry_count < max_retries`.
    Failed {
        /// Human-readable error (no secret content).
        error: String,
    },
    /// Task was cancelled before completion; not retried.
    Cancelled,
}

/// Payload for one incremental recompute task (stored as `checkpoint_json`).
///
/// The `dedup_key` makes enqueue idempotent: two watcher bursts for the same
/// file set collapse into one PENDING row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IncrementalTaskPayload {
    /// Stable dedup key (sorted changed-path list digest or reconciliation id).
    pub dedup_key: String,
    /// Repository-relative paths added or modified.
    pub upserts: Vec<String>,
    /// Repository-relative paths deleted.
    pub deletes: Vec<String>,
    /// Observed renames `(prior_path, new_path)` — hints only; verification
    /// happens at execution time.
    pub renames: Vec<(String, String)>,
    /// Whether this task was created by startup/offline reconciliation.
    pub from_reconciliation: bool,
}

// ---------------------------------------------------------------------------
// Enqueue
// ---------------------------------------------------------------------------

/// Insert a task row unless an identical PENDING task already exists.
///
/// Dedup compares `(task_type, repository_id, checkpoint_json)`; because
/// [`IncrementalTaskPayload`] carries a `dedup_key`, repeated identical
/// watcher bursts cannot create duplicate canonical mutations.
///
/// Returns the task id and whether a new row was created.
pub fn enqueue_task(
    conn: &Connection,
    id: &str,
    repository_id: Option<&str>,
    task_type: &str,
    priority: i64,
    checkpoint_json: &str,
    now_us: i64,
) -> Result<EnqueueOutcome, StorageError> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM ops_tasks
              WHERE task_type = ?1
                AND repository_id IS ?2
                AND checkpoint_json IS ?3
                AND state = 'PENDING'
              LIMIT 1",
            rusqlite::params![task_type, repository_id, checkpoint_json],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;

    if existing.is_some() {
        return Ok(EnqueueOutcome::AlreadyPending);
    }

    conn.execute(
        "INSERT INTO ops_tasks
             (id, repository_id, task_type, priority, state, checkpoint_json,
              retry_count, max_retries, created_at)
         VALUES (?1, ?2, ?3, ?4, 'PENDING', ?5, 0, 3, ?6)",
        rusqlite::params![
            id,
            repository_id,
            task_type,
            priority,
            checkpoint_json,
            now_us
        ],
    )?;
    Ok(EnqueueOutcome::Created)
}

// ---------------------------------------------------------------------------
// Claim / finish
// ---------------------------------------------------------------------------

/// Atomically claim the highest-priority PENDING task (PENDING → RUNNING).
///
/// Ordering: `priority DESC, created_at ASC, id ASC` — deterministic tie-break.
/// Must run inside the ambient writer transaction.
pub fn claim_next_pending_task(
    conn: &Connection,
    now_us: i64,
) -> Result<Option<ClaimedTask>, StorageError> {
    claim_pending(conn, now_us, None)
}

/// Claim the next PENDING task of exactly `task_type` (used by dedicated
/// workers such as the repository evictor).
pub fn claim_next_pending_task_of_type(
    conn: &Connection,
    task_type: &str,
    now_us: i64,
) -> Result<Option<ClaimedTask>, StorageError> {
    claim_pending(conn, now_us, Some(task_type))
}

/// `only_type = None` claims any type EXCEPT the dedicated-worker types
/// (`STALE_EVICTION`), so the indexing scheduler never takes — and fails —
/// work it has no handler for.
fn claim_pending(
    conn: &Connection,
    now_us: i64,
    only_type: Option<&str>,
) -> Result<Option<ClaimedTask>, StorageError> {
    use rusqlite::OptionalExtension;
    let claimed_id: Option<String> = conn
        .query_row(
            "UPDATE ops_tasks
                SET state = 'RUNNING', started_at = ?1
              WHERE id = (
                  SELECT id FROM ops_tasks
                   WHERE state = 'PENDING'
                     AND ((?2 IS NULL AND task_type != ?3) OR task_type = ?2)
                   ORDER BY priority DESC, created_at ASC, id ASC
                   LIMIT 1
              )
              RETURNING id",
            rusqlite::params![now_us, only_type, TASK_STALE_EVICTION],
            |r| r.get(0),
        )
        .optional()?;

    let Some(task_id) = claimed_id else {
        return Ok(None);
    };

    let task = conn.query_row(
        "SELECT id, task_type, repository_id, priority, checkpoint_json,
                retry_count, max_retries
           FROM ops_tasks WHERE id = ?1",
        rusqlite::params![task_id],
        |row| {
            Ok(ClaimedTask {
                id: row.get(0)?,
                task_type: row.get(1)?,
                repository_id: row.get(2)?,
                priority: row.get(3)?,
                checkpoint_json: row.get(4)?,
                retry_count: row.get(5)?,
                max_retries: row.get(6)?,
            })
        },
    )?;
    Ok(Some(task))
}

/// Record a task's terminal (or retry) outcome.
///
/// - [`TaskOutcome::Done`] → `DONE`, `completed_at` set.
/// - [`TaskOutcome::Failed`] → back to `PENDING` with `retry_count + 1` when
///   retries remain, otherwise `FAILED` with the error message.
/// - [`TaskOutcome::Cancelled`] → `CANCELLED`; not retried.
pub fn finish_task(
    conn: &Connection,
    task_id: &str,
    outcome: &TaskOutcome,
    now_us: i64,
) -> Result<(), StorageError> {
    match outcome {
        TaskOutcome::Done => {
            conn.execute(
                "UPDATE ops_tasks
                    SET state = 'DONE', completed_at = ?2, error_message = NULL
                  WHERE id = ?1",
                rusqlite::params![task_id, now_us],
            )?;
        }
        TaskOutcome::Failed { error } => {
            conn.execute(
                "UPDATE ops_tasks
                    SET state = CASE
                            WHEN retry_count < max_retries THEN 'PENDING'
                            ELSE 'FAILED'
                        END,
                        retry_count = retry_count + 1,
                        completed_at = CASE
                            WHEN retry_count < max_retries THEN NULL
                            ELSE ?2
                        END,
                        error_message = ?3
                  WHERE id = ?1",
                rusqlite::params![task_id, now_us, error],
            )?;
        }
        TaskOutcome::Cancelled => {
            conn.execute(
                "UPDATE ops_tasks
                    SET state = 'CANCELLED', completed_at = ?2
                  WHERE id = ?1",
                rusqlite::params![task_id, now_us],
            )?;
        }
    }
    Ok(())
}

/// Persist partial progress for a RUNNING task (crash recovery checkpoint).
pub fn set_task_checkpoint(
    conn: &Connection,
    task_id: &str,
    checkpoint_json: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE ops_tasks SET checkpoint_json = ?2 WHERE id = ?1",
        rusqlite::params![task_id, checkpoint_json],
    )?;
    Ok(())
}

/// Cancel a still-PENDING task (best effort; used on graceful shutdown).
pub fn cancel_pending_task(
    conn: &Connection,
    task_id: &str,
    now_us: i64,
) -> Result<bool, StorageError> {
    let n = conn.execute(
        "UPDATE ops_tasks SET state = 'CANCELLED', completed_at = ?2
          WHERE id = ?1 AND state = 'PENDING'",
        rusqlite::params![task_id, now_us],
    )?;
    Ok(n > 0)
}

// ---------------------------------------------------------------------------
// Recovery + status counts
// ---------------------------------------------------------------------------

/// Return one task left `RUNNING` (its outcome could not be recorded) to
/// `PENDING` without counting a retry. Returns whether a row changed.
pub fn reset_running_task(conn: &Connection, task_id: &str) -> Result<bool, StorageError> {
    let n = conn.execute(
        "UPDATE ops_tasks SET state = 'PENDING', started_at = NULL
          WHERE id = ?1 AND state = 'RUNNING'",
        rusqlite::params![task_id],
    )?;
    Ok(n > 0)
}

/// Reset tasks left `RUNNING` by a crash back to `PENDING`.
///
/// Returns the number of rows reset.  Idempotent: a second call resets zero
/// rows.  `retry_count` is deliberately NOT incremented — a crash between
/// claim and finish is not the task's fault.
pub fn recover_interrupted_tasks(conn: &Connection) -> Result<u64, StorageError> {
    let n = conn.execute(
        "UPDATE ops_tasks
            SET state = 'PENDING', started_at = NULL
          WHERE state = 'RUNNING'",
        [],
    )?;
    Ok(n as u64)
}

/// Default retention window, in days, for terminal (`DONE` / `FAILED` /
/// `CANCELLED`) `ops_tasks` rows before [`prune_terminal_tasks`] considers
/// them eligible for deletion.
pub const DEFAULT_TERMINAL_TASK_RETENTION_DAYS: u32 = 30;

/// Microseconds in one day (matches the `*_at` column unit used throughout
/// this schema).
const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

/// Permanently delete terminal `ops_tasks` rows older than the retention
/// window, mirroring the precedent at
/// `attic_semantic::store::SemanticStore::queue_prune_done` ("Remove DONE
/// rows entirely (bounded queue; done work needs no history)") — except here
/// three states are terminal (`DONE`, `FAILED`, `CANCELLED`; see
/// `migrations/0001_initial.sql`'s `ops_tasks.state` comment) rather than
/// just one.
///
/// `completed_at` is set by [`finish_task`] on every terminal transition (all
/// three [`TaskOutcome`] branches, and [`cancel_pending_task`]), so it is
/// used directly as the age basis. Non-terminal rows (`PENDING` / `RUNNING`)
/// are never touched, regardless of how old `created_at` is — only rows
/// already in a terminal state are eligible.
///
/// Returns the number of rows actually deleted.
pub fn prune_terminal_tasks(
    conn: &Connection,
    older_than_days: u32,
) -> Result<usize, StorageError> {
    let cutoff_us = now_us() - i64::from(older_than_days) * DAY_US;
    let n = conn.execute(
        "DELETE FROM ops_tasks
          WHERE state IN ('DONE', 'FAILED', 'CANCELLED')
            AND completed_at IS NOT NULL
            AND completed_at < ?1",
        rusqlite::params![cutoff_us],
    )?;
    Ok(n)
}

/// Count tasks per state for the MCP status tool.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct TaskCounts {
    /// Tasks waiting to be claimed.
    pub pending: i64,
    /// Tasks currently executing.
    pub running: i64,
    /// Tasks that exhausted their retries.
    pub failed: i64,
}

/// One RUNNING task that has exceeded the stuck-task age threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StuckTask {
    /// `ops_tasks.id`.
    pub task_id: String,
    /// `ops_tasks.task_type`.
    pub task_type: String,
    /// Repository root path when known, else the repository id string.
    pub repository: Option<String>,
    /// How long the task has been RUNNING.
    pub age_seconds: u64,
}

/// Conservative threshold used by `status` before a RUNNING incremental task
/// is called stuck. Detection only: no automatic reset/cancel happens.
pub const DEFAULT_STUCK_TASK_AGE_SECS: u64 = 60 * 60;

/// Read aggregate task counts.
pub fn get_task_counts(conn: &Connection) -> Result<TaskCounts, StorageError> {
    let counts = conn.query_row(
        "SELECT
             COALESCE(SUM(CASE WHEN state = 'PENDING'  THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN state = 'RUNNING'  THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN state = 'FAILED'   THEN 1 ELSE 0 END), 0)
           FROM ops_tasks",
        [],
        |r| {
            Ok(TaskCounts {
                pending: r.get(0)?,
                running: r.get(1)?,
                failed: r.get(2)?,
            })
        },
    )?;
    Ok(counts)
}

/// Task counts for ONE repository (tasks are keyed by `repository_id`).
pub fn get_task_counts_for_repo(
    conn: &Connection,
    repository_id: &str,
) -> Result<TaskCounts, StorageError> {
    let counts = conn.query_row(
        "SELECT
             COALESCE(SUM(CASE WHEN state = 'PENDING'  THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN state = 'RUNNING'  THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN state = 'FAILED'   THEN 1 ELSE 0 END), 0)
           FROM ops_tasks
          WHERE repository_id = ?1",
        [repository_id],
        |r| {
            Ok(TaskCounts {
                pending: r.get(0)?,
                running: r.get(1)?,
                failed: r.get(2)?,
            })
        },
    )?;
    Ok(counts)
}

fn list_stuck_running_tasks_query(
    conn: &Connection,
    older_than_secs: u64,
    now_us: i64,
    repository_id: Option<&str>,
) -> Result<Vec<StuckTask>, StorageError> {
    let cutoff_us = now_us.saturating_sub((older_than_secs as i64).saturating_mul(1_000_000));
    let sql = if repository_id.is_some() {
        "SELECT t.id, t.task_type, COALESCE(r.root_path, t.repository_id), t.started_at
           FROM ops_tasks t
           LEFT JOIN core_repositories r ON r.id = t.repository_id
          WHERE t.state = 'RUNNING'
            AND t.started_at IS NOT NULL
            AND t.started_at <= ?1
            AND t.repository_id = ?2
          ORDER BY t.started_at ASC, t.id ASC"
    } else {
        "SELECT t.id, t.task_type, COALESCE(r.root_path, t.repository_id), t.started_at
           FROM ops_tasks t
           LEFT JOIN core_repositories r ON r.id = t.repository_id
          WHERE t.state = 'RUNNING'
            AND t.started_at IS NOT NULL
            AND t.started_at <= ?1
          ORDER BY t.started_at ASC, t.id ASC"
    };
    let mut stmt = conn.prepare(sql)?;
    let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<StuckTask> {
        let started_at: i64 = row.get(3)?;
        Ok(StuckTask {
            task_id: row.get(0)?,
            task_type: row.get(1)?,
            repository: row.get(2)?,
            age_seconds: now_us.saturating_sub(started_at).max(0) as u64 / 1_000_000,
        })
    };
    let rows = if let Some(repo) = repository_id {
        stmt.query_map(rusqlite::params![cutoff_us, repo], mapper)?
    } else {
        stmt.query_map(rusqlite::params![cutoff_us], mapper)?
    };
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

/// RUNNING tasks older than `older_than_secs` across the whole workspace.
pub fn list_stuck_running_tasks(
    conn: &Connection,
    older_than_secs: u64,
) -> Result<Vec<StuckTask>, StorageError> {
    list_stuck_running_tasks_query(conn, older_than_secs, now_us(), None)
}

/// RUNNING tasks older than `older_than_secs` for one repository.
pub fn list_stuck_running_tasks_for_repo(
    conn: &Connection,
    repository_id: &str,
    older_than_secs: u64,
) -> Result<Vec<StuckTask>, StorageError> {
    list_stuck_running_tasks_query(conn, older_than_secs, now_us(), Some(repository_id))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn enqueue_is_idempotent_on_identical_payload() {
        let conn = migrated_conn();
        let repo_id = attic_core::RepositoryId::new_v4();
        crate::repository::repository::upsert_repository(&conn, &repo_id, "/repo", "r").unwrap();
        let repo_str = repo_id.to_string_repr();

        let payload = serde_json::to_string(&IncrementalTaskPayload {
            dedup_key: "k1".into(),
            upserts: vec!["a.rs".into()],
            deletes: vec![],
            renames: vec![],
            from_reconciliation: false,
        })
        .unwrap();

        let o1 = enqueue_task(
            &conn,
            "t-1",
            Some(&repo_str),
            TASK_INCREMENTAL_INDEX,
            50,
            &payload,
            1,
        )
        .unwrap();
        assert_eq!(o1, EnqueueOutcome::Created);
        let o2 = enqueue_task(
            &conn,
            "t-2",
            Some(&repo_str),
            TASK_INCREMENTAL_INDEX,
            50,
            &payload,
            2,
        )
        .unwrap();
        assert_eq!(o2, EnqueueOutcome::AlreadyPending);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ops_tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "duplicate burst must not create a second row");
    }

    #[test]
    fn claim_respects_priority_then_age() {
        let conn = migrated_conn();
        enqueue_task(&conn, "t-a", None, TASK_RECONCILIATION, 10, "{}", 1).unwrap();
        enqueue_task(
            &conn,
            "t-b",
            None,
            TASK_INCREMENTAL_INDEX,
            90,
            "{\"dedup_key\":\"lo\"}",
            1,
        )
        .unwrap();
        enqueue_task(
            &conn,
            "t-c",
            None,
            TASK_INCREMENTAL_INDEX,
            90,
            "{\"dedup_key\":\"hi\"}",
            2,
        )
        .unwrap();

        let t1 = claim_next_pending_task(&conn, 100)
            .unwrap()
            .expect("first claim");
        assert_eq!(t1.priority, 90);
        // Same priority → older created_at first ("lo" was created at 1).
        assert!(t1.checkpoint_json.unwrap().contains("lo"));

        let _ = finish_task(&conn, &t1.id, &TaskOutcome::Done, 101);
        // Remaining highest priority is the other priority-90 task ("hi").
        let t2 = claim_next_pending_task(&conn, 102)
            .unwrap()
            .expect("second claim");
        assert_eq!(t2.priority, 90);
        assert!(t2.checkpoint_json.unwrap().contains("hi"));
        let _ = finish_task(&conn, &t2.id, &TaskOutcome::Done, 103);

        let t3 = claim_next_pending_task(&conn, 104)
            .unwrap()
            .expect("third claim");
        assert_eq!(t3.priority, 10);

        let none = claim_next_pending_task(&conn, 105).unwrap();
        assert!(none.is_none(), "queue must be empty");
    }

    #[test]
    fn failed_task_is_retried_until_max() {
        let conn = migrated_conn();
        enqueue_task(
            &conn,
            "t-f",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"x\"}",
            1,
        )
        .unwrap();
        let t = claim_next_pending_task(&conn, 10).unwrap().unwrap();

        finish_task(
            &conn,
            &t.id,
            &TaskOutcome::Failed {
                error: "boom".into(),
            },
            11,
        )
        .unwrap();
        let (state, retries): (String, i64) = conn
            .query_row(
                "SELECT state, retry_count FROM ops_tasks WHERE id = ?1",
                [&t.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            state, "PENDING",
            "failed task with retries left goes back to PENDING"
        );
        assert_eq!(retries, 1);

        // Exhaust retries.  finish_task's CASE reads the PRE-increment
        // retry_count: fails at counts 1,2 → PENDING; count 3 → FAILED.
        for i in 0..3 {
            let t = claim_next_pending_task(&conn, 20 + i).unwrap().unwrap();
            finish_task(
                &conn,
                &t.id,
                &TaskOutcome::Failed {
                    error: "boom".into(),
                },
                21 + i,
            )
            .unwrap();
        }
        let none = claim_next_pending_task(&conn, 30).unwrap();
        assert!(none.is_none(), "exhausted task must not be re-queued");
        let state: String = conn
            .query_row("SELECT state FROM ops_tasks WHERE id = ?1", [&t.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "FAILED", "exhausted retries must land in FAILED");
    }

    #[test]
    fn recover_interrupted_tasks_is_idempotent() {
        let conn = migrated_conn();
        enqueue_task(
            &conn,
            "t-r",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"r\"}",
            1,
        )
        .unwrap();
        let t = claim_next_pending_task(&conn, 10).unwrap().unwrap();

        let n1 = recover_interrupted_tasks(&conn).unwrap();
        assert_eq!(n1, 1);
        let n2 = recover_interrupted_tasks(&conn).unwrap();
        assert_eq!(n2, 0, "second recovery run must be a no-op");

        let (state, started): (String, Option<i64>) = conn
            .query_row(
                "SELECT state, started_at FROM ops_tasks WHERE id = ?1",
                [&t.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "PENDING");
        assert_eq!(started, None);
    }

    #[test]
    fn task_counts_for_repo_are_scoped() {
        let conn = migrated_conn();
        conn.execute_batch(
            "INSERT INTO core_repositories (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at)
               VALUES ('ra','/a','a',1,1,0,0), ('rb','/b','b',1,1,0,0);",
        )
        .unwrap();
        enqueue_task(
            &conn,
            "t1",
            Some("ra"),
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"1\"}",
            1,
        )
        .unwrap();
        enqueue_task(
            &conn,
            "t2",
            Some("ra"),
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"2\"}",
            2,
        )
        .unwrap();
        let a = get_task_counts_for_repo(&conn, "ra").unwrap();
        let b = get_task_counts_for_repo(&conn, "rb").unwrap();
        assert_eq!(a.pending, 2);
        assert_eq!(b.pending, 0);
        assert_eq!(get_task_counts(&conn).unwrap().pending, 2);
    }

    #[test]
    fn reset_running_task_returns_only_that_task_to_pending() {
        let conn = migrated_conn();
        for id in ["t-a", "t-b"] {
            enqueue_task(
                &conn,
                id,
                None,
                TASK_INCREMENTAL_INDEX,
                50,
                &format!("{{\"dedup_key\":\"{id}\"}}"),
                1,
            )
            .unwrap();
        }
        let a = claim_next_pending_task(&conn, 10).unwrap().unwrap();
        let b = claim_next_pending_task(&conn, 11).unwrap().unwrap();
        assert!(reset_running_task(&conn, &a.id).unwrap());
        assert!(!reset_running_task(&conn, &a.id).unwrap(), "idempotent");
        let state = |id: &str| -> String {
            conn.query_row("SELECT state FROM ops_tasks WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(state(&a.id), "PENDING");
        assert_eq!(state(&b.id), "RUNNING");
    }

    #[test]
    fn stuck_running_tasks_are_reported_with_repo_and_age() {
        let conn = migrated_conn();
        let repo_id = attic_core::RepositoryId::new_v4();
        crate::repository::repository::upsert_repository(&conn, &repo_id, "/repo", "repo").unwrap();
        let repo = repo_id.to_string_repr();

        enqueue_task(
            &conn,
            "t-old-running",
            Some(&repo),
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"old\"}",
            1_000_000,
        )
        .unwrap();
        enqueue_task(
            &conn,
            "t-fresh-running",
            Some(&repo),
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"fresh\"}",
            2_000_000,
        )
        .unwrap();
        let _old = claim_next_pending_task(&conn, 20_000_000).unwrap().unwrap();
        let _fresh = claim_next_pending_task(&conn, 70_000_000).unwrap().unwrap();

        let stuck = list_stuck_running_tasks_query(&conn, 60, 100_000_000, Some(&repo)).unwrap();
        assert_eq!(stuck.len(), 1, "{stuck:?}");
        assert_eq!(stuck[0].task_id, "t-old-running");
        assert_eq!(stuck[0].task_type, TASK_INCREMENTAL_INDEX);
        assert_eq!(stuck[0].repository.as_deref(), Some("/repo"));
        assert_eq!(stuck[0].age_seconds, 80);
    }

    #[test]
    fn prune_terminal_tasks_prunes_only_old_terminal_rows() {
        let conn = migrated_conn();
        let old_cutoff_us =
            now_us() - (i64::from(DEFAULT_TERMINAL_TASK_RETENTION_DAYS) + 1) * DAY_US;

        // Old DONE row: must be pruned.
        enqueue_task(
            &conn,
            "t-old-done",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"a\"}",
            1,
        )
        .unwrap();
        let old_done = claim_next_pending_task(&conn, 1).unwrap().unwrap();
        finish_task(&conn, &old_done.id, &TaskOutcome::Done, old_cutoff_us).unwrap();

        // Old FAILED (retries exhausted) row: must be pruned.
        enqueue_task(
            &conn,
            "t-old-failed",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"b\"}",
            1,
        )
        .unwrap();
        let mut failing = claim_next_pending_task(&conn, 1).unwrap().unwrap();
        for _ in 0..4 {
            finish_task(
                &conn,
                &failing.id,
                &TaskOutcome::Failed {
                    error: "boom".into(),
                },
                old_cutoff_us,
            )
            .unwrap();
            if let Some(t) = claim_next_pending_task(&conn, old_cutoff_us).unwrap() {
                failing = t;
            }
        }
        let old_failed_state: String = conn
            .query_row(
                "SELECT state FROM ops_tasks WHERE id = ?1",
                [&failing.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            old_failed_state, "FAILED",
            "fixture must actually reach FAILED"
        );

        // Old CANCELLED row: must be pruned.
        enqueue_task(
            &conn,
            "t-old-cancelled",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"c\"}",
            1,
        )
        .unwrap();
        let old_cancelled = claim_next_pending_task(&conn, 1).unwrap().unwrap();
        finish_task(
            &conn,
            &old_cancelled.id,
            &TaskOutcome::Cancelled,
            old_cutoff_us,
        )
        .unwrap();

        // Recent DONE row: must survive.
        enqueue_task(
            &conn,
            "t-recent-done",
            None,
            TASK_INCREMENTAL_INDEX,
            50,
            "{\"dedup_key\":\"d\"}",
            1,
        )
        .unwrap();
        let recent_done = claim_next_pending_task(&conn, 1).unwrap().unwrap();
        finish_task(&conn, &recent_done.id, &TaskOutcome::Done, now_us()).unwrap();

        // Non-terminal PENDING row: must never be touched, regardless of age.
        enqueue_task(
            &conn,
            "t-pending",
            None,
            TASK_RECONCILIATION,
            50,
            "{\"dedup_key\":\"pending\"}",
            old_cutoff_us,
        )
        .unwrap();

        // Non-terminal RUNNING row: must never be touched, regardless of age.
        enqueue_task(
            &conn,
            "t-running",
            None,
            TASK_RECONCILIATION,
            90,
            "{\"dedup_key\":\"running\"}",
            old_cutoff_us,
        )
        .unwrap();
        let running = claim_next_pending_task(&conn, old_cutoff_us)
            .unwrap()
            .unwrap();

        let total_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM ops_tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total_before, 6);

        let pruned = prune_terminal_tasks(&conn, DEFAULT_TERMINAL_TASK_RETENTION_DAYS).unwrap();
        assert_eq!(
            pruned, 3,
            "old DONE + old FAILED + old CANCELLED must be pruned"
        );

        let remaining: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM ops_tasks ORDER BY id")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(
            remaining.len(),
            3,
            "recent DONE + pending + running must survive"
        );
        assert!(remaining.contains(&recent_done.id));
        assert!(remaining.contains(&running.id));
        assert!(
            remaining
                .iter()
                .any(|id| id != &recent_done.id && id != &running.id),
            "the still-PENDING row must also survive"
        );
    }
}
