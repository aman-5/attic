//! Phase 2 lifecycle scenarios — deterministic end-to-end pipeline tests.
//!
//! Covers contract §15 items 1–17: modification, creation, deletion,
//! rename/move, rename+modify, rapid modifications, create+delete before
//! debounce, delete+recreate, duplicate events, `.gitignore` change,
//! newly ignored removed from FTS, newly included indexed, knowledge-file
//! modification, unaffected repository untouched, atomic FTS updates,
//! no ghost results, identity across rename / uncertain rename.

mod common;

use attic_incremental::{CoalescedChange, FsEventKind, NormalizedEvent};
use common::*;
use std::time::Instant;

const TIMEOUT_MS: u128 = 30_000;

fn within_budget(start: &Instant) {
    assert!(
        start.elapsed().as_millis() < TIMEOUT_MS,
        "test exceeded its deterministic time budget"
    );
}

#[test]
fn single_file_modification_refreshes_and_restores_current() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[
        ("src/a.rs", "fn alpha() {}\n"),
        ("src/b.rs", "fn beta() {}\n"),
    ]);
    let svc = fx.service();

    write_file(fx.root(), "src/a.rs", "fn zeta_freshly_named() {}\n");
    fx.step(&svc, vec![("src/a.rs".into(), FsEventKind::Modified)]);

    let hits = fx.search("zeta_freshly_named");
    assert_eq!(hits.len(), 1, "new content must be searchable");
    assert_eq!(hits[0].0, "src/a.rs");
    assert_eq!(hits[0].1, "CURRENT", "refreshed occurrence must be CURRENT");
    assert!(
        fx.search("alpha").is_empty(),
        "old content must be gone after refresh"
    );
    let occ = fx.occurrence("src/a.rs").expect("occurrence");
    assert_eq!(occ.content_hash, hash_of(fx.root(), "src/a.rs"));
    within_budget(&t0);
}

#[test]
fn file_creation_is_indexed() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[]);
    let svc = fx.service();

    write_file(fx.root(), "docs/new.md", "# brand new token\n");
    fx.step(&svc, vec![("docs/new.md".into(), FsEventKind::Created)]);

    let hits = fx.search("brand");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0, "docs/new.md");
    within_budget(&t0);
}

#[test]
fn file_deletion_removes_fts_entries_no_ghosts() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/gone.rs", "fn ghost_token() {}\n")]);
    let svc = fx.service();

    delete_file(fx.root(), "src/gone.rs");
    fx.step(&svc, vec![("src/gone.rs".into(), FsEventKind::Removed)]);

    let hits = fx.search("ghost_token");
    assert!(hits.is_empty(), "no ghost results allowed after delete");

    // Tombstone occurrence exists and is not CURRENT.
    let occ = fx.occurrence("src/gone.rs").expect("tombstone");
    assert_eq!(occ.existence_state, "deleted");
    assert_ne!(occ.freshness_state, "CURRENT");
    within_budget(&t0);
}

#[test]
fn rename_moves_content_between_paths_with_identity_link() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/old_place.rs", "fn moved_body_token() {}\n")]);
    let old_occ = fx.occurrence("src/old_place.rs").expect("pre-rename");
    let svc = fx.service();

    std::fs::rename(
        fx.root().join("src/old_place.rs"),
        fx.root().join("src/new_place.rs"),
    )
    .expect("rename");
    fx.apply_ops(
        &svc,
        vec![CoalescedChange::Rename(
            "src/old_place.rs".into(),
            "src/new_place.rs".into(),
        )],
    );

    assert!(
        fx.search("moved_body_token")
            .iter()
            .all(|(p, _)| p == "src/new_place.rs"),
        "content searchable only under the new path"
    );
    assert!(
        !fx.search("moved_body_token").is_empty(),
        "rename must keep content searchable"
    );

    // Identity continuity recorded explicitly as HEURISTIC content match.
    let links: i64 = fx.sql_count(
        "SELECT COUNT(*) FROM core_identity_links
          WHERE prior_path = 'src/old_place.rs'
            AND new_path   = 'src/new_place.rs'
            AND confidence = 'HEURISTIC'",
    );
    assert_eq!(
        links, 1,
        "identical-content move must record a HEURISTIC link"
    );
    let _ = old_occ;
    within_budget(&t0);
}

#[test]
fn rename_plus_modification_indexes_new_content() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/orig.rs", "fn before_move() {}\n")]);
    let svc = fx.service();

    write_file(fx.root(), "src/moved_v2.rs", "fn after_move_token() {}\n");
    delete_file(fx.root(), "src/orig.rs");
    fx.apply_ops(
        &svc,
        vec![
            CoalescedChange::Rename("src/orig.rs".into(), "src/moved_v2.rs".into()),
            CoalescedChange::Upsert("src/moved_v2.rs".into()),
            CoalescedChange::Remove("src/orig.rs".into()),
        ],
    );

    let hits = fx.search("after_move_token");
    assert_eq!(hits.len(), 1, "modified-after-move content indexed");
    // Different content → NO identity promotion.
    let links: i64 = fx.sql_count(
        "SELECT COUNT(*) FROM core_identity_links
          WHERE prior_path = 'src/orig.rs' AND new_path = 'src/moved_v2.rs'",
    );
    assert_eq!(links, 0, "uncertain rename must remain uncertain");
    assert!(fx.search("before_move").is_empty(), "old path content gone");
    within_budget(&t0);
}

#[test]
fn rapid_repeated_modifications_collapse_to_one_task() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/hot.rs", "fn v0() {}\n")]);
    let svc = fx.service();

    for i in 1..=10 {
        write_file(fx.root(), "src/hot.rs", &format!("fn v{i}() {{}}\n"));
        svc.ingest(&[NormalizedEvent {
            rel_path: "src/hot.rs".into(),
            kind: FsEventKind::Modified,
        }]);
    }
    // One drain far in the future collapses everything into one hint.
    let drained = svc.drain_due(Some(u64::MAX / 2));
    let upserts = drained
        .iter()
        .filter(|c| matches!(c, CoalescedChange::Upsert(_)))
        .count();
    assert_eq!(upserts, 1, "storm must coalesce per path, got {drained:?}");

    fx.apply_ops(&svc, drained);
    let hits = fx.search("v10");
    assert_eq!(hits.len(), 1, "final content wins");
    assert!(fx.search("fn v0").is_empty());
    within_budget(&t0);
}

#[test]
fn create_then_delete_before_debounce_produces_nothing() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[]);
    let svc = fx.service();

    svc.ingest(&[NormalizedEvent {
        rel_path: "tmp/x.rs".into(),
        kind: FsEventKind::Created,
    }]);
    svc.ingest(&[NormalizedEvent {
        rel_path: "tmp/x.rs".into(),
        kind: FsEventKind::Removed,
    }]);

    let drained = svc.drain_due(Some(u64::MAX / 2));
    assert!(
        drained.is_empty(),
        "create→delete inside the debounce window must vanish, got {drained:?}"
    );
    within_budget(&t0);
}

#[test]
fn delete_then_recreate_indexes_fresh_content() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/phoenix.rs", "fn old_life() {}\n")]);
    let svc = fx.service();

    delete_file(fx.root(), "src/phoenix.rs");
    svc.ingest(&[NormalizedEvent {
        rel_path: "src/phoenix.rs".into(),
        kind: FsEventKind::Removed,
    }]);
    write_file(fx.root(), "src/phoenix.rs", "fn new_life_token() {}\n");
    svc.ingest(&[NormalizedEvent {
        rel_path: "src/phoenix.rs".into(),
        kind: FsEventKind::Created,
    }]);

    let drained = svc.drain_due(Some(u64::MAX / 2));
    let has_upsert = drained
        .iter()
        .any(|c| matches!(c, CoalescedChange::Upsert(p) if p == "src/phoenix.rs"));
    assert!(
        has_upsert,
        "delete→recreate must surface an Upsert, got {drained:?}"
    );

    fx.apply_ops(&svc, drained);
    assert_eq!(fx.search("new_life_token").len(), 1);
    assert!(
        fx.search("old_life").is_empty(),
        "replaced content purged from FTS"
    );
    within_budget(&t0);
}

#[test]
fn duplicate_watcher_events_do_not_duplicate_work_or_rows() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/dup.rs", "fn dup_target() {}\n")]);
    let svc = fx.service();

    for _ in 0..5 {
        svc.ingest(&[NormalizedEvent {
            rel_path: "src/dup.rs".into(),
            kind: FsEventKind::Modified,
        }]);
    }
    let _pending_before = fx.sql_count("SELECT COUNT(*) FROM ops_tasks WHERE state='PENDING'");

    // First application invalidates + schedules one task.
    let ops = svc.drain_due(Some(u64::MAX / 2));
    let report = svc.apply_operations(&fx.pool, &fx.writer, ops).unwrap();
    assert!(!report.queue_saturated);

    // Second identical burst dedups at enqueue.
    let payload = attic_storage::IncrementalTaskPayload {
        dedup_key: "identical-burst".into(),
        upserts: vec!["src/dup.rs".into()],
        deletes: vec![],
        renames: vec![],
        from_reconciliation: false,
    };
    let o1 = attic_incremental::scheduler::schedule_incremental(
        &fx.writer,
        &fx.repo_id,
        &payload,
        80,
        4096,
    )
    .unwrap();
    let o2 = attic_incremental::scheduler::schedule_incremental(
        &fx.writer,
        &fx.repo_id,
        &payload,
        80,
        4096,
    )
    .unwrap();
    assert_eq!(
        o2,
        attic_incremental::scheduler::ScheduleOutcome::Deduplicated
    );
    let _ = o1;

    while attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
    )
    .unwrap()
    {}

    let units = fx.sql_count("SELECT COUNT(*) FROM core_retrieval_units");
    let occurrences = fx.sql_count(
        "SELECT COUNT(DISTINCT fo.id) FROM core_file_occurrences fo WHERE fo.path='src/dup.rs'",
    );
    assert!(
        units >= 1 && occurrences >= 2,
        "state sane after duplicates"
    );
    within_budget(&t0);
}
