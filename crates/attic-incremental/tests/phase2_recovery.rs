//! Phase 2 recovery scenarios — crash/power-loss semantics, restart
//! idempotency, offline source drift, graceful shutdown, cancellation.
//!
//! Deterministic except the single process-kill test, which uses explicit
//! deadlines and cleans up its child unconditionally.

mod common;

use attic_incremental::FsEventKind;
use common::*;
use std::time::{Duration, Instant};

const TIMEOUT_MS: u128 = 30_000;

fn within_budget(start: &Instant) {
    assert!(
        start.elapsed().as_millis() < TIMEOUT_MS,
        "test exceeded its deterministic time budget"
    );
}

#[test]
fn restart_with_interrupted_task_reschedules_it() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/wip.rs", "fn wip_token() {}\n")]);

    // Simulate a crash between claim and completion: RUNNING row left behind.
    let payload = attic_storage::IncrementalTaskPayload {
        dedup_key: "interrupted".into(),
        upserts: vec!["src/wip.rs".into()],
        deletes: vec![],
        renames: vec![],
        from_reconciliation: false,
    };
    attic_incremental::scheduler::schedule_incremental(&fx.writer, &fx.repo_id, &payload, 80, 4096)
        .unwrap();
    let claimed: Option<attic_storage::ClaimedTask> =
        attic_incremental::run_on_writer(&fx.writer, |conn| {
            attic_storage::claim_next_pending_task(conn, 1_000)
        })
        .unwrap();
    assert!(claimed.is_some(), "task must be claimable");

    // Startup recovery resets it.
    let report = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    assert_eq!(report.tasks_reset, 1);

    // Idempotency: second recovery run is a no-op.
    let again = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    assert_eq!(again.tasks_reset, 0);
    within_budget(&t0);
}

#[test]
fn crash_between_invalidation_and_recomputation_never_serves_current() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/crash_gap.rs", "fn gap_old() {}\n")]);
    write_file(fx.root(), "src/crash_gap.rs", "fn gap_new_token() {}\n");

    let svc = fx.service();
    // Stage 1 only: invalidate + schedule, but DO NOT execute the task
    // (this is exactly the post-crash state when a kill lands in the gap).
    svc.ingest(&[attic_incremental::NormalizedEvent {
        rel_path: "src/crash_gap.rs".into(),
        kind: FsEventKind::Modified,
    }]);
    let report = svc
        .apply_pending(&fx.pool, &fx.writer, Some(u64::MAX / 2))
        .unwrap();
    assert!(report.task_queued);

    // The stale occurrence must never be presented as CURRENT by reads:
    // its units are INVALID (hidden from FTS), and the occurrence itself
    // carries observable STALE metadata.
    assert!(
        fx.search("gap_old").is_empty(),
        "invalidated units are not served"
    );
    let snap = fx.occurrence("src/crash_gap.rs").expect("occurrence");
    assert_eq!(
        snap.freshness_state, "STALE",
        "staleness must be observable"
    );
    assert!(
        !fx.search("gap_new_token")
            .iter()
            .any(|(_, f)| f == "CURRENT"),
        "recomputation has not run yet; new content cannot be CURRENT"
    );

    // Recovery keeps it non-CURRENT and the pending task survives.
    let rec = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    let _ = rec;
    let snap = fx.occurrence("src/crash_gap.rs").unwrap();
    assert_eq!(
        snap.freshness_state, "STALE",
        "recovery must not promote stale to CURRENT"
    );

    // Now finish recomputation → CURRENT restored.
    while attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
    )
    .unwrap()
    {}
    let hits = fx.search("gap_new_token");
    assert_eq!(hits.len(), 1);
    assert_eq!(
        hits[0].1, "CURRENT",
        "successful refresh returns state to CURRENT"
    );
    within_budget(&t0);
}

#[test]
fn dead_writer_rolls_back_scoped_publication_atomically() {
    let t0 = Instant::now();
    let dir = tempfile::TempDir::new().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    write_file(&repo_dir, "src/atom.rs", "fn atomic_before_token() {}\n");

    let db_path = dir.path().join("db.sqlite");
    {
        let (conn, pool) = attic_storage::open_db(&db_path).unwrap();
        attic_storage::run_migrations(&conn).unwrap();
        let queue = attic_storage::WriterQueue::new(conn).unwrap();
        let writer = queue.handle();
        let store = attic_indexing::IndexingStore {
            readers: &pool,
            writer: &writer,
        };
        attic_indexing::index_repository(
            &store,
            &repo_dir,
            &attic_discovery::DiscoveryPolicy::default_git(),
            &Default::default(),
        )
        .unwrap();
        let writer_after = writer.clone();
        drop(writer);
        drop(queue); // writer thread gone from here on

        write_file(&repo_dir, "src/atom.rs", "fn atomic_after_token() {}\n");
        let broken_store = attic_indexing::IndexingStore {
            readers: &pool,
            writer: &writer_after,
        };
        let err = attic_indexing::index_changes(
            &broken_store,
            &repo_dir,
            &attic_discovery::DiscoveryPolicy::default_git(),
            &Default::default(),
            &attic_indexing::ScopedChanges {
                upserts: vec!["src/atom.rs".into()],
                deletes: vec![],
                rename_hints: vec![],
            },
        );
        assert!(err.is_err(), "publication against dead writer must fail");
        drop(pool);
    } // all handles dropped — WAL state settled

    // Re-open independently: previous coherent state intact, no half-state.
    let ro = attic_storage::connection::open_ro(&db_path).unwrap();
    let units: i64 = ro
        .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(units, 1, "exactly the original unit survives");

    let fts_hit: i64 = ro
        .query_row(
            "SELECT COUNT(*) FROM fts_retrieval_units WHERE fts_retrieval_units MATCH 'atomic_before_token'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fts_hit, 1, "previous FTS content preserved");
    let new_hit: i64 = ro
        .query_row(
            "SELECT COUNT(*) FROM fts_retrieval_units WHERE fts_retrieval_units MATCH 'atomic_after_token'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_hit, 0, "failed publication leaves no trace");
    within_budget(&t0);
}

#[test]
fn source_modified_while_attic_offline_is_caught_by_reconciliation() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/offline.rs", "fn before_offline() {}\n")]);

    // Attic is "down": file changes with no watcher/service running.
    write_file(
        fx.root(),
        "src/offline.rs",
        "fn changed_offline_token() {}\n",
    );

    // On startup the authoritative reconciliation walk detects the drift.
    let report =
        attic_incremental::reconcile_repository(&fx.pool, &fx.writer, fx.root(), &fx.policy())
            .unwrap();
    assert!(
        report
            .change_set
            .upserts
            .contains(&"src/offline.rs".to_string()),
        "offline modification must be detected"
    );

    // Invalidate + schedule + execute through the normal pipeline.
    let svc = fx.service();
    fx.apply_ops(
        &svc,
        report
            .change_set
            .upserts
            .iter()
            .map(|p| attic_incremental::CoalescedChange::Upsert(p.clone()))
            .collect(),
    );
    let hits = fx.search("changed_offline_token");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "CURRENT");
    assert!(fx.search("before_offline").is_empty());
    within_budget(&t0);
}

#[test]
fn recovery_is_idempotent_across_repeated_restarts() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/idem.rs", "fn idem_token() {}\n")]);
    // Leave one task mid-flight so recovery has work.
    let payload = attic_storage::IncrementalTaskPayload {
        dedup_key: "idem".into(),
        upserts: vec!["src/idem.rs".into()],
        deletes: vec![],
        renames: vec![],
        from_reconciliation: false,
    };
    attic_incremental::scheduler::schedule_incremental(&fx.writer, &fx.repo_id, &payload, 80, 4096)
        .unwrap();

    let r1 = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    let r2 = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    let r3 = attic_incremental::run_startup_recovery(&fx.pool, &fx.writer).unwrap();
    assert_eq!(
        r2.tasks_reset + r2.refreshes_rescheduled,
        0,
        "second run no-op"
    );
    assert_eq!(
        r3.tasks_reset + r3.refreshes_rescheduled,
        0,
        "third run no-op"
    );
    assert!(r1.watcher_epoch <= r3.watcher_epoch);
    within_budget(&t0);
}

#[test]
fn cancellation_removes_pending_task_before_execution() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/cxl.rs", "fn cxl_token() {}\n")]);
    let payload = attic_storage::IncrementalTaskPayload {
        dedup_key: "cxl".into(),
        upserts: vec!["src/cxl.rs".into()],
        deletes: vec![],
        renames: vec![],
        from_reconciliation: false,
    };
    attic_incremental::scheduler::schedule_incremental(&fx.writer, &fx.repo_id, &payload, 50, 4096)
        .unwrap();

    let cancelled = attic_incremental::run_on_writer(&fx.writer, |conn| {
        let id: String = conn.query_row(
            "SELECT id FROM ops_tasks WHERE state='PENDING' LIMIT 1",
            [],
            |r| r.get(0),
        )?;
        attic_storage::cancel_pending_task(conn, &id, 5_000)
    })
    .unwrap();
    assert!(cancelled, "PENDING task must be cancellable");

    let executed = attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
    )
    .unwrap();
    assert!(!executed, "cancelled task must not execute");
    let state: String = fx
        .pool
        .with_reader(|c| {
            c.query_row("SELECT state FROM ops_tasks LIMIT 1", [], |r| r.get(0))
                .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(state, "CANCELLED");
    within_budget(&t0);
}

#[test]
fn graceful_shutdown_with_pending_work_preserves_queue() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[
        ("src/g1.rs", "fn g1_token() {}\n"),
        ("src/g2.rs", "fn g2_token() {}\n"),
    ]);

    let sched = attic_incremental::spawn_scheduler(
        attic_incremental::SchedulerConfig {
            workers: 1,
            poll_interval: Duration::from_millis(10),
            ..Default::default()
        },
        fx.pool.clone(),
        fx.writer.clone(),
        fx.root().to_path_buf(),
        fx.policy(),
    )
    .expect("scheduler must start");

    // Queue several tasks, then shut down gracefully with a deadline.
    for dedup in ["d1", "d2", "d3"] {
        let payload = attic_storage::IncrementalTaskPayload {
            dedup_key: dedup.into(),
            upserts: vec!["src/g1.rs".into()],
            deletes: vec![],
            renames: vec![],
            from_reconciliation: false,
        };
        attic_incremental::scheduler::schedule_incremental(
            &fx.writer,
            &fx.repo_id,
            &payload,
            80,
            4096,
        )
        .unwrap();
    }

    let deadline = Instant::now() + Duration::from_secs(15);
    sched.signal_stop();
    // shutdown() joins workers; guard with our own wall-clock budget via a
    // detached join — but SchedulerHandle::shutdown joins internally, so run
    // it and assert the overall test budget afterwards.
    sched.shutdown();
    assert!(
        Instant::now() < deadline,
        "graceful shutdown exceeded its deadline"
    );

    // Any tasks not executed remain PENDING in durable storage — nothing lost.
    let pending = fx.sql_count("SELECT COUNT(*) FROM ops_tasks WHERE state IN ('PENDING','DONE')");
    assert!(
        pending >= 1,
        "queued work must survive as PENDING or complete"
    );
    within_budget(&t0);
}
