//! Background repository evictor: deletes the stored data of repositories
//! removed from the workspace (`STALE_EVICTION` tasks).
//!
//! `workspace remove` enqueues one task per removed repository. This worker
//! is the only consumer of that task type (the indexing scheduler never
//! claims it). Each task deletes `attic.db` rows in small FK-ordered writer
//! transactions, then the repository's `semantic.db` rows. Membership is
//! re-checked inside every canonical delete transaction and again inside the
//! semantic delete transaction, so a root re-added before the next delete
//! phase begins cancels the task and keeps the remaining data. Tasks survive
//! restarts and resume idempotently.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use attic_storage::WriterQueueHandle;
use attic_storage::ops_tasks::{
    TASK_STALE_EVICTION, TaskOutcome, claim_next_pending_task_of_type, enqueue_task, finish_task,
};
use attic_storage::repo_eviction::evict_repository_step_if_inactive;

const POLL: Duration = Duration::from_secs(2);

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EvictionTestHookPoint {
    BeforeCanonicalStep,
    BeforeSemanticCleanup,
}

#[cfg(test)]
type EvictionTestHook = Arc<dyn Fn(EvictionTestHookPoint, &str, &str) + Send + Sync>;

#[cfg(test)]
static EVICTION_TEST_HOOK: OnceLock<Mutex<Option<EvictionTestHook>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct EvictionTestHookGuard;

#[cfg(test)]
impl Drop for EvictionTestHookGuard {
    fn drop(&mut self) {
        if let Some(slot) = EVICTION_TEST_HOOK.get() {
            *slot.lock().unwrap() = None;
        }
    }
}

#[cfg(test)]
pub(crate) fn install_test_hook(hook: EvictionTestHook) -> EvictionTestHookGuard {
    let slot = EVICTION_TEST_HOOK.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = Some(hook);
    EvictionTestHookGuard
}

#[cfg(test)]
fn fire_test_hook(point: EvictionTestHookPoint, repo: &str, root: &str) {
    let hook = EVICTION_TEST_HOOK
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()));
    if let Some(hook) = hook {
        hook(point, repo, root);
    }
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
    _active_roots: &RwLock<Vec<PathBuf>>,
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
        #[cfg(test)]
        fire_test_hook(EvictionTestHookPoint::BeforeCanonicalStep, repo, root);
        let (tx, rx) = std::sync::mpsc::channel();
        let repo_owned = repo.to_string();
        let root_owned = root.to_string();
        let res = writer.send(move |c| {
            let step = if root_owned.is_empty() {
                Some(attic_storage::repo_eviction::evict_repository_step(
                    c,
                    &repo_owned,
                )?)
            } else {
                evict_repository_step_if_inactive(c, &repo_owned, Path::new(&root_owned))?
            };
            let _ = tx.send(step);
            Ok(())
        });
        if let Err(e) = res {
            return TaskOutcome::Failed {
                error: format!("canonical eviction step failed: {e}"),
            };
        }
        match rx.try_recv() {
            Ok(Some(step)) => {
                files += step.files;
                units += step.units;
                if step.complete {
                    break;
                }
            }
            Ok(None) => {
                tracing::info!(repository_id = repo, "root re-added; eviction cancelled");
                return TaskOutcome::Cancelled;
            }
            Err(_) => {
                return TaskOutcome::Failed {
                    error: "canonical eviction step returned no result".into(),
                };
            }
        }
    }
    #[cfg(test)]
    fire_test_hook(EvictionTestHookPoint::BeforeSemanticCleanup, repo, root);
    let semantic_rows = match semantic {
        Some(s) if root.is_empty() => match s.store.evict_repository(repo) {
            Err(e) => {
                return TaskOutcome::Failed {
                    error: format!("semantic eviction failed: {e}"),
                };
            }
            Ok(n) => n,
        },
        Some(s) => match s.store.evict_repository_if_inactive(repo, Path::new(root)) {
            Ok(Some(n)) => n,
            Ok(None) => {
                tracing::info!(
                    repository_id = repo,
                    "root re-added before semantic cleanup; eviction cancelled"
                );
                return TaskOutcome::Cancelled;
            }
            Err(e) => {
                return TaskOutcome::Failed {
                    error: format!("semantic eviction failed: {e}"),
                };
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use attic_discovery::DiscoveryPolicy;
    use attic_indexing::{IndexOptions, IndexingStore};
    use attic_storage::{
        WriterQueue, get_repository_stats, lookup_repository_by_root_path, open_db,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        pool: attic_storage::DbPool,
        writer: WriterQueueHandle,
        _queue: WriterQueue,
        root: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("attic.db");
        let (conn, pool) = open_db(&db_path).unwrap();
        attic_storage::run_migrations(&conn).unwrap();
        let queue = WriterQueue::new(conn).unwrap();
        let writer = queue.handle();
        let root = tmp.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "pub fn keep_me() {}\n").unwrap();
        Fixture {
            _tmp: tmp,
            pool,
            writer,
            _queue: queue,
            root,
        }
    }

    #[test]
    fn readded_root_cancels_eviction_before_next_step_and_preserves_repository_id() {
        let fx = fixture();
        let store = IndexingStore {
            readers: &fx.pool,
            writer: &fx.writer,
        };
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let first = attic_indexing::index_repository(&store, &fx.root, &policy, &opts).unwrap();
        fx.writer
            .send(|conn| attic_storage::repo_eviction::sync_workspace_membership(conn, &[]))
            .unwrap();

        let repo_id = first.repository_id.clone();
        let hook_writer = fx.writer.clone();
        let hook_root = fx.root.clone();
        let fired = Arc::new(AtomicBool::new(false));
        let fired_once = Arc::clone(&fired);
        let _hook = install_test_hook(Arc::new(move |point, repo, _root| {
            if point == EvictionTestHookPoint::BeforeCanonicalStep
                && repo == repo_id
                && !fired_once.swap(true, Ordering::SeqCst)
            {
                hook_writer
                    .send({
                        let hook_root = hook_root.clone();
                        move |conn| {
                            attic_storage::repo_eviction::sync_workspace_membership(
                                conn,
                                std::slice::from_ref(&hook_root),
                            )
                        }
                    })
                    .unwrap();
            }
        }));

        let outcome = evict(
            &fx.writer,
            None,
            &RwLock::new(Vec::new()),
            &first.repository_id,
            &fx.root.to_string_lossy(),
        );
        assert!(matches!(outcome, TaskOutcome::Cancelled));
        assert!(fired.load(Ordering::SeqCst));

        let lookup_id = fx
            .pool
            .with_reader(|conn| lookup_repository_by_root_path(conn, &fx.root.to_string_lossy()))
            .unwrap()
            .map(|id| id.to_string())
            .expect("repository row must survive the cancelled eviction");
        assert_eq!(lookup_id, first.repository_id);

        let stats = fx.pool.with_reader(get_repository_stats).unwrap();
        let kept = stats.iter().find(|s| s.id == first.repository_id).unwrap();
        assert!(
            kept.file_count >= 1,
            "repo files must survive when the root is re-added before the delete step"
        );

        let second = attic_indexing::index_repository(&store, &fx.root, &policy, &opts).unwrap();
        assert_eq!(
            second.repository_id, first.repository_id,
            "re-adding the same root must reuse the surviving repository_id"
        );
    }
}
