//! Background repository evictor: deletes the stored data of repositories
//! removed from the workspace (`STALE_EVICTION` tasks).
//!
//! `workspace remove` enqueues one task per removed repository. This worker
//! is the only consumer of that task type (the indexing scheduler never
//! claims it). Each task deletes `attic.db` rows in small FK-ordered writer
//! transactions, then the repository's `semantic.db` rows. If the root is
//! added back before the task finishes, the task is cancelled and the data
//! kept. Tasks survive restarts and resume idempotently.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use attic_storage::WriterQueueHandle;
use attic_storage::ops_tasks::{
    TASK_STALE_EVICTION, TaskOutcome, claim_next_pending_task_of_type, enqueue_task, finish_task,
};
use attic_storage::repo_eviction::evict_repository_step;

const POLL: Duration = Duration::from_secs(2);

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// True when `root` is (again) covered by live workspace membership.
fn is_active(active_roots: &RwLock<Vec<PathBuf>>, root: &str) -> bool {
    let key = path_key(Path::new(root));
    active_roots.read().is_ok_and(|roots| {
        roots.iter().any(|r| {
            let rk = path_key(r);
            key == rk || key.starts_with(&format!("{rk}/"))
        })
    })
}

/// Queue eviction of one removed repository. Idempotent per repository.
pub fn enqueue_eviction(
    writer: &WriterQueueHandle,
    repository_id: &str,
    root_path: &Path,
) -> Result<(), attic_storage::StorageError> {
    let payload = serde_json::json!({
        "repository_id": repository_id,
        "root_path": root_path.to_string_lossy(),
    })
    .to_string();
    let task_id = format!("evict-{repository_id}-{}", now_us());
    writer.send(move |c| {
        enqueue_task(
            c,
            &task_id,
            None,
            TASK_STALE_EVICTION,
            10,
            &payload,
            now_us(),
        )
        .map(|_| ())
    })
}

/// Start the evictor thread. It runs for the life of the process.
pub fn spawn_evictor(
    pool: attic_storage::DbPool,
    writer: WriterQueueHandle,
    semantic: Option<Arc<attic_retrieval::semantic::SemanticStack>>,
    active_roots: Arc<RwLock<Vec<PathBuf>>>,
) {
    // A crash mid-eviction leaves the task RUNNING; steps are idempotent,
    // so simply make it claimable again (writes only if such a row exists).
    let _ = writer.send(|c| {
        c.execute(
            "UPDATE ops_tasks SET state = 'PENDING'
              WHERE task_type = 'STALE_EVICTION' AND state = 'RUNNING'",
            [],
        )?;
        Ok(())
    });
    let spawned = std::thread::Builder::new()
        .name("attic-evictor".into())
        .spawn(move || {
            loop {
                if !run_one(&pool, &writer, semantic.as_deref(), &active_roots) {
                    std::thread::sleep(POLL);
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("repository evictor could not start: {e}");
    }
}

/// Claim and run one eviction task. Returns false when none was pending.
fn run_one(
    pool: &attic_storage::DbPool,
    writer: &WriterQueueHandle,
    semantic: Option<&attic_retrieval::semantic::SemanticStack>,
    active_roots: &RwLock<Vec<PathBuf>>,
) -> bool {
    // Poll on a READ connection: every writer commit bumps the write
    // generation, which wakes the semantic reconcile scan. Claiming through
    // the writer only when work exists keeps an idle evictor invisible.
    let pending = pool
        .with_reader(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM ops_tasks
                  WHERE task_type = 'STALE_EVICTION' AND state = 'PENDING')",
                [],
                |r| r.get::<_, bool>(0),
            )?)
        })
        .unwrap_or(false);
    if !pending {
        return false;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let claimed = writer.send(move |c| {
        let _ = tx.send(claim_next_pending_task_of_type(
            c,
            TASK_STALE_EVICTION,
            now_us(),
        )?);
        Ok(())
    });
    let Some(task) = claimed.ok().and_then(|_| rx.try_recv().ok()).flatten() else {
        return false;
    };
    let payload: serde_json::Value =
        serde_json::from_str(task.checkpoint_json.as_deref().unwrap_or("{}")).unwrap_or_default();
    let repo = payload["repository_id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let root = payload["root_path"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let outcome = evict(writer, semantic, active_roots, &repo, &root);
    let task_id = task.id.clone();
    let _ = writer.send(move |c| finish_task(c, &task_id, &outcome, now_us()));
    true
}

fn evict(
    writer: &WriterQueueHandle,
    semantic: Option<&attic_retrieval::semantic::SemanticStack>,
    active_roots: &RwLock<Vec<PathBuf>>,
    repo: &str,
    root: &str,
) -> TaskOutcome {
    if repo.is_empty() {
        return TaskOutcome::Failed {
            error: "eviction task without repository_id".into(),
        };
    }
    let (mut files, mut units) = (0usize, 0usize);
    loop {
        // Re-added while pending/running: keep the data.
        if !root.is_empty() && is_active(active_roots, root) {
            tracing::info!(repository_id = repo, "root re-added; eviction cancelled");
            return TaskOutcome::Cancelled;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let repo_owned = repo.to_string();
        let res = writer.send(move |c| {
            let _ = tx.send(evict_repository_step(c, &repo_owned)?);
            Ok(())
        });
        if let Err(e) = res {
            return TaskOutcome::Failed {
                error: format!("canonical eviction step failed: {e}"),
            };
        }
        match rx.try_recv() {
            Ok(step) => {
                files += step.files;
                units += step.units;
                if step.complete {
                    break;
                }
            }
            Err(_) => {
                return TaskOutcome::Failed {
                    error: "canonical eviction step returned no result".into(),
                };
            }
        }
    }
    let semantic_rows = match semantic.map(|s| s.store.evict_repository(repo)) {
        Some(Err(e)) => {
            return TaskOutcome::Failed {
                error: format!("semantic eviction failed: {e}"),
            };
        }
        Some(Ok(n)) => n,
        None => 0,
    };
    tracing::info!(
        repository_id = repo,
        files,
        units,
        semantic_rows,
        "removed repository data evicted"
    );
    TaskOutcome::Done
}
