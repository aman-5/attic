//! Tests for the post-review runtime fixes:
//! pump lifetime, git-aware eligibility, saturation→reconciliation,
//! bootstrap fail-closed, reconciliation-origin priority.

mod common;

use attic_incremental::{FsEventKind, IncrementalService, NormalizedEvent};
use common::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

const TEST_BUDGET_MS: u128 = 30_000;
const WAIT_BUDGET: Duration = Duration::from_secs(8);
/// Far-future virtual timestamp: guarantees the debounce window elapsed.
const FAR_FUTURE: u64 = u64::MAX / 4;

fn within_budget(t0: &Instant) {
    assert!(t0.elapsed().as_millis() < TEST_BUDGET_MS);
}

fn wait_for(mut check: impl FnMut() -> bool) -> bool {
    let dl = Instant::now() + WAIT_BUDGET;
    while Instant::now() < dl {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

// ---------------------------------------------------------------------------
// 1. Native watcher stays alive past the former artificial 300 s lifetime —
//    proven structurally (default has NO lifetime) and behaviourally (two
//    deliveries spaced well apart; injectable lifetime auto-stops).
// ---------------------------------------------------------------------------

#[test]
fn native_pump_has_no_production_lifetime_and_stays_live() {
    let t0 = Instant::now();
    // Default: no lifetime (structural proof).
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let svc_default =
        IncrementalService::new(&repo, attic_discovery::DiscoveryPolicy::default_non_git());
    assert_eq!(
        svc_default.pump_lifetime(),
        None,
        "production watcher must run until explicit shutdown"
    );

    // Behavioural: real watcher delivers twice, ≥1 s apart, and reports live.
    let db = dir.path().join("db.sqlite");
    let (conn, pool) = attic_storage::open_db(&db).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let policy = attic_discovery::DiscoveryPolicy::default_git();

    write_file(&repo, "seed.rs", "fn seed_token() {}\n");
    attic_indexing::index_repository(
        &attic_indexing::IndexingStore {
            readers: &pool,
            writer: &writer,
        },
        &repo,
        &policy,
        &Default::default(),
    )
    .unwrap();

    let svc = Arc::new(IncrementalService::new(&repo, policy.clone()).with_quiet_period_ms(50));
    let mut watch = svc
        .start_incremental_watch(pool.clone(), writer.clone())
        .expect("native watcher on temp dir");
    assert_eq!(watch.mode(), attic_incremental::WatchMode::NativeWatcher);
    // Production always runs scheduler workers alongside the watcher.
    let _sched = attic_incremental::spawn_scheduler(
        attic_incremental::SchedulerConfig {
            workers: 1,
            poll_interval: Duration::from_millis(50),
            ..Default::default()
        },
        pool.clone(),
        writer.clone(),
        repo.clone(),
        policy.clone(),
        None,
    )
    .expect("scheduler start");
    // Allow the backend to arm before producing events.
    std::thread::sleep(Duration::from_millis(400));

    for round in ["first_late_token", "second_late_token"] {
        write_file(&repo, "late.rs", &format!("fn {round}() {{}}\n"));
        let hit = wait_for(|| {
            pool.with_reader(|c| {
                attic_storage::fts_search(
                    c,
                    &attic_storage::FtsSearchParams {
                        query: round,
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 5,
                    },
                )
            })
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        });
        assert!(hit, "watcher must deliver {round}");
        // Space deliveries apart: still alive long after any tick.
        std::thread::sleep(Duration::from_millis(1100));
    }

    assert!(
        watch.running(),
        "pump must remain running without an artificial deadline"
    );
    watch.stop();
    within_budget(&t0);
}

#[test]
fn injectable_pump_lifetime_auto_stops_for_tests() {
    let t0 = Instant::now();
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let (conn, pool) = attic_storage::open_db(dir.path().join("db.sqlite")).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();

    let mut svc =
        IncrementalService::new(&repo, attic_discovery::DiscoveryPolicy::default_non_git())
            .with_quiet_period_ms(5);
    // Injectable lifetime: deterministic auto-stop instead of sleeping 300 s.
    svc.pump_lifetime_for_tests = Some(Duration::from_millis(120));
    let svc = Arc::new(svc);

    let watch = svc
        .start_incremental_watch(pool, writer)
        .expect("watcher start");
    assert!(
        wait_for(|| !watch.running()),
        "injectable lifetime must stop the pump deterministically"
    );
    within_budget(&t0);
}

// ---------------------------------------------------------------------------
// 2+3+4. Git-ignored events never index; nested gitignore/negation parity
// with discovery; explicit re-inclusion still updates.
// ---------------------------------------------------------------------------

#[test]
fn git_ignored_events_never_become_indexed() {
    let t0 = Instant::now();
    let dir = tempfile::TempDir::new().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(repo_dir.join("ignored_dir")).unwrap();
    write_file(&repo_dir, ".gitignore", "ignored_dir/\n");
    write_file(&repo_dir, "src/ok.rs", "fn ok_token() {}\n");
    git_init_isolated(&repo_dir);

    let (conn, pool) = attic_storage::open_db(dir.path().join("db.sqlite")).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    attic_indexing::index_repository(
        &attic_indexing::IndexingStore {
            readers: &pool,
            writer: &writer,
        },
        &repo_dir,
        &policy,
        &Default::default(),
    )
    .unwrap();

    // Watcher event fires for a GIT-IGNORED path.  Authoritative discovery
    // must win over the hint.
    write_file(
        &repo_dir,
        "ignored_dir/secretish.rs",
        "fn ignored_token() {}\n",
    );
    let svc = Arc::new(IncrementalService::new(&repo_dir, policy.clone()).with_quiet_period_ms(5));
    svc.ingest(&[NormalizedEvent {
        rel_path: "ignored_dir/secretish.rs".into(),
        kind: FsEventKind::Created,
    }]);
    svc.apply_pending(&pool, &writer, Some(FAR_FUTURE)).unwrap();
    while attic_incremental::run_next_task_synchronously(&pool, &writer, &repo_dir, &policy, None)
        .unwrap()
    {}

    let hits = pool
        .with_reader(|c| {
            attic_storage::fts_search(
                c,
                &attic_storage::FtsSearchParams {
                    query: "ignored_token",
                    repository_id: None,
                    file_type: None,
                    language: None,
                    max_results: 5,
                },
            )
        })
        .unwrap();
    assert!(hits.is_empty(), "git-ignored event must NOT index the file");

    let occ: i64 = pool
        .with_reader(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM core_file_occurrences WHERE path LIKE 'ignored_dir/%'",
                [],
                |r| r.get(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(occ, 0, "no occurrence row may exist for the ignored path");
    within_budget(&t0);
}

#[test]
fn nested_gitignore_and_negation_match_discovery_exactly() {
    let t0 = Instant::now();
    let dir = tempfile::TempDir::new().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(repo_dir.join("sub")).unwrap();
    // Root: ignore all .log EXCEPT keep.log (negation).
    write_file(&repo_dir, ".gitignore", "*.log\n!keep.log\n");
    // Nested: inside sub/, ignore *.tmp EXCEPT important.tmp.
    write_file(&repo_dir, "sub/.gitignore", "*.tmp\n!important.tmp\n");
    write_file(&repo_dir, "keep.log", "keep log content\n");
    write_file(&repo_dir, "drop.log", "drop log content\n");
    write_file(&repo_dir, "sub/important.tmp", "important tmp content\n");
    write_file(&repo_dir, "sub/discard.tmp", "discard tmp content\n");
    git_init_isolated(&repo_dir);

    let (conn, pool) = attic_storage::open_db(dir.path().join("db.sqlite")).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let policy = attic_discovery::DiscoveryPolicy::default_git();

    // Discovery's verdict is the authority.
    let discovery = attic_discovery::discover(&repo_dir, &policy).unwrap();
    let discovered: std::collections::BTreeSet<String> = discovery
        .entries
        .iter()
        .map(|e| e.repo_relative.clone())
        .collect();
    assert!(discovered.contains("keep.log"), "negation keeps keep.log");
    assert!(!discovered.contains("drop.log"));
    assert!(discovered.contains("sub/important.tmp"), "nested negation");
    assert!(!discovered.contains("sub/discard.tmp"));

    // Index everything discoverable, then fire events for ALL four files.
    attic_indexing::index_repository(
        &attic_indexing::IndexingStore {
            readers: &pool,
            writer: &writer,
        },
        &repo_dir,
        &policy,
        &Default::default(),
    )
    .unwrap();
    let svc = Arc::new(IncrementalService::new(&repo_dir, policy.clone()).with_quiet_period_ms(5));
    for p in [
        "keep.log",
        "drop.log",
        "sub/important.tmp",
        "sub/discard.tmp",
    ] {
        svc.ingest(&[NormalizedEvent {
            rel_path: p.into(),
            kind: FsEventKind::Modified,
        }]);
    }
    svc.apply_pending(&pool, &writer, Some(FAR_FUTURE)).unwrap();
    while attic_incremental::run_next_task_synchronously(&pool, &writer, &repo_dir, &policy, None)
        .unwrap()
    {}

    // Indexed set for these paths must EQUAL discovery's verdict.
    let indexed: Vec<String> = pool
        .with_reader(|c| {
            let mut stmt = c.prepare(
                "SELECT DISTINCT fo.path FROM core_file_occurrences fo
                  WHERE fo.path IN ('keep.log','drop.log','sub/important.tmp','sub/discard.tmp')
                    AND fo.existence_state != 'deleted'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .unwrap();
    let indexed: std::collections::BTreeSet<_> = indexed.into_iter().collect();
    let expected: std::collections::BTreeSet<_> = discovered
        .intersection(&std::collections::BTreeSet::from([
            "keep.log".into(),
            "drop.log".into(),
            "sub/important.tmp".into(),
            "sub/discard.tmp".into(),
        ]))
        .cloned()
        .collect();
    assert_eq!(
        indexed, expected,
        "incremental outcome must match authoritative discovery exactly"
    );
    within_budget(&t0);
}

#[test]
fn explicit_reincluded_vendor_path_receives_updates() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[]);

    // Policy re-includes vendored content (default-ignored by walker).
    let mut policy = fx.policy();
    policy
        .attic_include_rules
        .push(attic_discovery::GlobRule::include("vendor/**"));

    let svc = Arc::new(IncrementalService::new(fx.root(), policy.clone()).with_quiet_period_ms(5));
    write_file(
        fx.root(),
        "vendor/lib/v.rs",
        "fn vendored_update_token() {}\n",
    );
    svc.ingest(&[NormalizedEvent {
        rel_path: "vendor/lib/v.rs".into(),
        kind: FsEventKind::Modified,
    }]);
    svc.apply_pending(&fx.pool, &fx.writer, Some(FAR_FUTURE))
        .unwrap();
    while attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        None,
    )
    .unwrap()
    {}

    let hits = fx.search("vendored_update_token");
    let occ = fx.occurrence("vendor/lib/v.rs");
    let units = fx.sql_count(
        "SELECT COUNT(*) FROM core_retrieval_units WHERE retrieval_text LIKE '%vendored_update_token%'",
    );
    let tasks: Vec<(String, String)> = fx
        .pool
        .with_reader(|c| {
            let mut s = c.prepare(
                "SELECT state, COALESCE(error_message,'') FROM ops_tasks ORDER BY created_at",
            )?;
            let mut rows = s.query([])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                out.push((r.get::<_, String>(0)?, r.get::<_, String>(1)?));
            }
            Ok(out)
        })
        .unwrap_or_default();
    eprintln!("PROBE-VENDOR occ={occ:?} units={units} tasks={tasks:?} hits={hits:?}");
    assert_eq!(hits.len(), 1, "re-included vendor path must get updates");
    assert_eq!(hits[0].1, "CURRENT");
    within_budget(&t0);
}

// ---------------------------------------------------------------------------
// 5+6. Raw-queue saturation schedules a DEDUPLICATED reconciliation.
// ---------------------------------------------------------------------------

#[test]
fn saturation_schedules_deduplicated_reconciliation() {
    let t0 = Instant::now();
    let dir = tempfile::TempDir::new().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let (conn, pool) = attic_storage::open_db(dir.path().join("db.sqlite")).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let svc = Arc::new(
        IncrementalService::new(&repo, attic_discovery::DiscoveryPolicy::default_non_git())
            .with_quiet_period_ms(5),
    );

    // Simulate three saturation bursts through the same production entry
    // point the pump uses.
    use std::sync::atomic::{AtomicU64, Ordering};
    let dropped = Arc::new(AtomicU64::new(0));
    for _ in 0..3 {
        dropped.store(1, Ordering::SeqCst);
        let n = svc.note_raw_drops(&dropped);
        assert_eq!(n, 1);
        if n > 0 {
            attic_incremental::recovery::schedule_reconciliation(&writer).unwrap();
        }
    }
    assert!(svc.reconciliation_required());

    let recon_tasks: i64 = pool
        .with_reader(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM ops_tasks WHERE task_type='RECONCILIATION' AND state='PENDING'",
                [],
                |r| r.get(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(
        recon_tasks, 1,
        "repeated saturation must deduplicate to ONE reconciliation task"
    );
    within_budget(&t0);
}

// ---------------------------------------------------------------------------
// 7. Reconciliation-origin tasks carry reconciliation priority/flag.
// ---------------------------------------------------------------------------

#[test]
fn reconciliation_origin_gets_reconciliation_priority() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/origin.rs", "fn before_origin() {}\n")]);

    // Offline drift, then a RECONCILIATION task runs and spawns follow-up work.
    write_file(fx.root(), "src/origin.rs", "fn after_origin_token() {}\n");
    attic_incremental::recovery::schedule_reconciliation(&fx.writer).unwrap();
    let ran = attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        None,
    )
    .unwrap();
    assert!(ran);

    // The spawned INCREMENTAL_INDEX task must carry reconciliation origin.
    let (priority, payload): (i64, String) = fx
        .pool
        .with_reader(|c| {
            c.query_row(
                "SELECT priority, checkpoint_json FROM ops_tasks
                  WHERE task_type='INCREMENTAL_INDEX' AND state='PENDING'
                  ORDER BY created_at DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(
        priority,
        attic_incremental::scheduler::PRIORITY_RECONCILE,
        "reconciliation-origin work must use reconciliation priority"
    );
    let parsed: attic_storage::IncrementalTaskPayload = serde_json::from_str(&payload).unwrap();
    assert!(
        parsed.from_reconciliation,
        "payload must preserve reconciliation origin"
    );

    // Finish the work: converged CURRENT.
    while attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        None,
    )
    .unwrap()
    {}
    let hits = fx.search("after_origin_token");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "CURRENT");
    within_budget(&t0);
}
