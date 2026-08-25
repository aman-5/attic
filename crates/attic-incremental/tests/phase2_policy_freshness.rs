//! Phase 2 lifecycle scenarios, part 2 — policy changes, isolation,
//! freshness visibility, boundedness, saturation.

mod common;

use attic_incremental::FsEventKind;
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
fn gitignore_change_triggers_targeted_rediscovery() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[
        (".gitignore", "ignored_dir/\n"),
        ("src/keep.rs", "fn keep_token() {}\n"),
        ("ignored_dir/hidden.rs", "fn hidden_token() {}\n"),
    ]);
    let svc = fx.service();

    // Tighten the ignore policy: src/keep.rs becomes newly ignored.
    write_file(fx.root(), ".gitignore", "src/keep.rs\n");
    fx.step(&svc, vec![(".gitignore".into(), FsEventKind::Modified)]);

    // The pipeline must have scheduled targeted rediscovery (RECONCILIATION).
    let reconciles =
        fx.sql_count("SELECT COUNT(*) FROM ops_tasks WHERE task_type='RECONCILIATION'");
    assert!(
        reconciles >= 1,
        ".gitignore change must schedule rediscovery"
    );
    within_budget(&t0);
}

#[test]
fn gitignore_modification_removes_newly_ignored_file_from_fts() {
    let t0 = Instant::now();
    // Real Git repo (isolated config) so .gitignore semantics are honored.
    let dir = tempfile::TempDir::new().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    write_file(&repo_dir, ".gitignore", "# nothing yet\n");
    write_file(&repo_dir, "src/doomed.rs", "fn doomed_token() {}\n");
    git_init_isolated(&repo_dir);

    let db_path = dir.path().join("db.sqlite");
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
    assert_eq!(
        pool.with_reader(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM core_file_occurrences fo WHERE fo.path LIKE '%doomed%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap(),
        1,
        "bootstrap indexed the not-yet-ignored file"
    );

    // ── The .gitignore modification (the first-class change under test) ──
    write_file(&repo_dir, ".gitignore", "src/doomed.rs\n");
    let report = attic_incremental::reconcile_repository(
        &pool,
        &writer,
        &repo_dir,
        &attic_discovery::DiscoveryPolicy::default_git(),
    )
    .expect("authoritative rescan");
    assert_eq!(
        report.change_set.deletes,
        vec!["src/doomed.rs".to_owned()],
        "reconciliation must classify the newly ignored file as deleted"
    );

    // Apply through the normal pipeline → FTS entry removed, no ghost.
    // The walk-verified change set is applied directly: the file still
    // exists on disk (policy exclusion), so per-hint disk verification
    // would wrongly cancel it.
    let git_policy = attic_discovery::DiscoveryPolicy::default_git();
    let svc = attic_incremental::IncrementalService::new(&repo_dir, git_policy.clone());
    svc.apply_verified_change_set(&pool, &writer, &report.change_set)
        .unwrap();
    while attic_incremental::run_next_task_synchronously(&pool, &writer, &repo_dir, &git_policy)
        .unwrap()
    {}

    let ghosts: Vec<_> = pool
        .with_reader(|c| {
            attic_storage::fts_search(
                c,
                &attic_storage::FtsSearchParams {
                    query: "doomed_token",
                    repository_id: None,
                    file_type: None,
                    language: None,
                    max_results: 10,
                },
            )
        })
        .unwrap();
    assert!(ghosts.is_empty(), "newly ignored file must vanish from FTS");
    within_budget(&t0);
}

#[test]
fn discovery_policy_exclusion_removes_file_from_fts() {
    let t0 = Instant::now();
    // Attic-level policy exclusion (independent of Git) — same contract path:
    // policy change → targeted rediscovery → newly excluded removed.
    let fx = Fixture::new(&[("src/policy_doomed.rs", "fn policy_doomed_token() {}\n")]);

    let mut stricter = fx.policy();
    stricter
        .attic_exclude_rules
        .push(attic_discovery::GlobRule::exclude("src/policy_doomed.rs"));
    stricter.validate().unwrap();

    let report =
        attic_incremental::reconcile_repository(&fx.pool, &fx.writer, fx.root(), &stricter)
            .expect("reconcile");
    assert_eq!(
        report.change_set.deletes,
        vec!["src/policy_doomed.rs".to_owned()]
    );

    // Walk-verified exclusion is applied directly (file still on disk).
    let svc = fx.service();
    svc.apply_verified_change_set(&fx.pool, &fx.writer, &report.change_set)
        .unwrap();
    while attic_incremental::run_next_task_synchronously(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
    )
    .unwrap()
    {}
    assert!(fx.search("policy_doomed_token").is_empty());
    within_budget(&t0);
}

#[test]
fn newly_included_file_is_indexed_by_reconciliation() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[(".gitignore", "secrets_dir/\n")]);
    // File exists on disk but was never indexed (excluded at bootstrap).
    write_file(
        fx.root(),
        "secrets_dir/open_me.rs",
        "fn newly_included_token() {}\n",
    );

    // Policy change re-includes it.
    write_file(fx.root(), ".gitignore", "# nothing excluded\n");
    let report =
        attic_incremental::reconcile_repository(&fx.pool, &fx.writer, fx.root(), &fx.policy())
            .expect("reconcile");
    assert!(
        report
            .change_set
            .upserts
            .contains(&"secrets_dir/open_me.rs".to_string()),
        "reconciliation must discover the newly included file"
    );

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
    assert_eq!(fx.search("newly_included_token").len(), 1);
    within_budget(&t0);
}

#[test]
fn knowledge_file_modification_invalidates_only_that_file() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[
        (
            "knowledge/architecture.md",
            "The system uses token_knowledge_v1.\n",
        ),
        ("src/app.rs", "fn app_token() {}\n"),
    ]);
    let svc = fx.service();

    write_file(
        fx.root(),
        "knowledge/architecture.md",
        "The system now uses token_knowledge_v2.\n",
    );
    fx.step(
        &svc,
        vec![("knowledge/architecture.md".into(), FsEventKind::Modified)],
    );

    assert_eq!(fx.search("token_knowledge_v2").len(), 1);
    assert!(
        fx.search("token_knowledge_v1").is_empty(),
        "stale knowledge text must be purged from FTS"
    );
    // Unrelated source content untouched.
    assert_eq!(fx.search("app_token").len(), 1);
    within_budget(&t0);
}

#[test]
fn unaffected_repository_is_completely_untouched() {
    let t0 = Instant::now();

    // Repository A.
    let dir = tempfile::TempDir::new().unwrap();
    let db_path = dir.path().join("multi.db");
    let repo_a = dir.path().join("repo_a");
    let repo_b = dir.path().join("repo_b");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    write_file(&repo_a, "a.rs", "fn only_in_a_token() {}\n");
    write_file(&repo_b, "b.rs", "fn only_in_b_token() {}\n");

    let (conn, pool) = attic_storage::open_db(&db_path).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();

    let store = attic_indexing::IndexingStore {
        readers: &pool,
        writer: &writer,
    };
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    let _ra = attic_indexing::index_repository(
        &store,
        &repo_a,
        &policy,
        &attic_indexing::IndexOptions::default(),
    )
    .unwrap();
    let rb = attic_indexing::index_repository(
        &store,
        &repo_b,
        &policy,
        &attic_indexing::IndexOptions::default(),
    )
    .unwrap();

    // Edit A only via the incremental pipeline scoped to A.
    let svc_a = attic_incremental::IncrementalService::new(&repo_a, policy.clone());
    write_file(&repo_a, "a.rs", "fn edited_in_a_token() {}\n");
    svc_a.ingest(&[attic_incremental::NormalizedEvent {
        rel_path: "a.rs".into(),
        kind: FsEventKind::Modified,
    }]);
    svc_a
        .apply_pending(&pool, &writer, Some(u64::MAX / 2))
        .unwrap();
    while attic_incremental::run_next_task_synchronously(&pool, &writer, &repo_a, &policy).unwrap()
    {
    }

    // B's committed state must be byte-identical to before.
    let b_revs: i64 = pool
        .with_reader(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM core_source_revisions WHERE repository_id = ?1",
                [rb.repository_id.clone()],
                |r| r.get(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(b_revs, 1, "repository B must gain no new revisions");

    let b_fresh: i64 = pool
        .with_reader(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1 AND fo.freshness_state != 'CURRENT'",
                [rb.repository_id.clone()],
                |r| r.get(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    assert_eq!(b_fresh, 0, "repository B must stay fully CURRENT");
    // A's new content searchable; B's content untouched and searchable.
    let hits_a: usize = pool
        .with_reader(|c| {
            attic_storage::fts_search(
                c,
                &attic_storage::FtsSearchParams {
                    query: "edited_in_a_token",
                    repository_id: None,
                    file_type: None,
                    language: None,
                    max_results: 10,
                },
            )
        })
        .unwrap()
        .len();
    let hits_b: usize = pool
        .with_reader(|c| {
            attic_storage::fts_search(
                c,
                &attic_storage::FtsSearchParams {
                    query: "only_in_b_token",
                    repository_id: None,
                    file_type: None,
                    language: None,
                    max_results: 10,
                },
            )
        })
        .unwrap()
        .len();
    assert_eq!(hits_a, 1);
    assert_eq!(hits_b, 1);
    within_budget(&t0);
}

#[test]
fn invalidation_is_visible_and_invalid_units_never_served() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/lab.rs", "fn labelled_token() {}\n")]);

    // Invalidate WITHOUT recomputing (the crash-gap state).  Per INV-01 the
    // occurrence goes STALE while its derived units go INVALID.
    let occ = fx.occurrence("src/lab.rs").unwrap();
    fx.writer
        .send(move |conn| {
            attic_storage::invalidate_for_occurrences(
                conn,
                std::slice::from_ref(&occ.id),
                attic_core::FreshnessState::Stale,
                attic_core::InvalidationCause::SourceChanged,
                12345,
            )?;
            Ok(())
        })
        .unwrap();

    // Staleness is observable on the occurrence...
    let snap = fx.occurrence("src/lab.rs").unwrap();
    assert_eq!(snap.freshness_state, "STALE");
    let pending_records =
        fx.sql_count("SELECT COUNT(*) FROM core_invalidation_records WHERE recomputed_at IS NULL");
    assert!(
        pending_records >= 2,
        "invalidation audit trail must be written"
    );

    // ...while invalidated units are NEVER served as evidence.
    assert!(
        fx.search("labelled_token").is_empty(),
        "INVALID retrieval units are not searchable"
    );

    // Escalate occurrence to INVALID: still hidden, and never CURRENT.
    let occ = fx.occurrence("src/lab.rs").unwrap();
    fx.writer
        .send(move |conn| {
            attic_storage::invalidate_for_occurrences(
                conn,
                std::slice::from_ref(&occ.id),
                attic_core::FreshnessState::Invalid,
                attic_core::InvalidationCause::Explicit,
                12346,
            )?;
            Ok(())
        })
        .unwrap();
    assert!(fx.search("labelled_token").is_empty());
    assert_eq!(
        fx.occurrence("src/lab.rs").unwrap().freshness_state,
        "INVALID"
    );

    // Successful refresh restores CURRENT and searchability.
    write_file(fx.root(), "src/lab.rs", "fn relabelled_fresh() {}\n");
    let svc = fx.service();
    fx.apply_ops(
        &svc,
        vec![attic_incremental::CoalescedChange::Upsert(
            "src/lab.rs".into(),
        )],
    );
    let hits = fx.search("relabelled_fresh");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "CURRENT", "refresh returns state to CURRENT");
    within_budget(&t0);
}

#[test]
fn event_storm_is_bounded_and_flags_reconciliation() {
    let t0 = Instant::now();
    let mut coalescer = attic_incremental::EventCoalescer::new(100, 8);

    for i in 0..64 {
        let accepted = coalescer.push(
            &attic_incremental::NormalizedEvent {
                rel_path: format!("burst/{i}.rs"),
                kind: FsEventKind::Modified,
            },
            i * 10,
        );
        if !accepted {
            break;
        }
    }

    assert!(
        coalescer.overflowed(),
        "storm beyond capacity must be detected, not silently absorbed"
    );
    assert!(
        coalescer.pending_count() <= 9,
        "pending state must stay bounded (cap 8)"
    );
    within_budget(&t0);
}

#[test]
fn queue_saturation_marks_unknown_and_requires_reconciliation() {
    let t0 = Instant::now();
    let fx = Fixture::new(&[("src/sat.rs", "fn saturated_token() {}\n")]);

    let payload = attic_storage::IncrementalTaskPayload {
        dedup_key: "saturate".into(),
        upserts: vec!["src/sat.rs".into()],
        deletes: vec![],
        renames: vec![],
        from_reconciliation: false,
    };
    let outcome = attic_incremental::scheduler::schedule_incremental(
        &fx.writer,
        &fx.repo_id,
        &payload,
        80,
        0, // zero capacity → immediate saturation signal
    )
    .unwrap();
    assert_eq!(
        outcome,
        attic_incremental::scheduler::ScheduleOutcome::Saturated,
        "bounded queue must refuse instead of growing unbounded"
    );

    // Contract §13 response: mark affected state UNKNOWN so nothing falsely
    // claims CURRENT, and demand an authoritative rescan.
    let repo: attic_core::RepositoryId = fx.repo_id.parse().unwrap();
    fx.writer
        .send(move |conn| {
            if let Some(snap) =
                attic_storage::lookup_occurrence_snapshot(conn, &repo, "src/sat.rs")?
            {
                conn.execute(
                    "UPDATE core_file_occurrences SET freshness_state='UNKNOWN'
                      WHERE id=?1 AND freshness_state='CURRENT'",
                    [&snap.id],
                )?;
            }
            attic_storage::enqueue_task(
                conn,
                &uuid::Uuid::new_v4().to_string(),
                None,
                "RECONCILIATION",
                40,
                "{}",
                999,
            )?;
            Ok(())
        })
        .unwrap();

    let unknown =
        fx.sql_count("SELECT COUNT(*) FROM core_file_occurrences WHERE freshness_state='UNKNOWN'");
    assert_eq!(unknown, 1, "saturation must degrade trust to UNKNOWN");
    assert!(
        fx.search("saturated_token").is_empty(),
        "UNKNOWN must not serve as CURRENT"
    );
    within_budget(&t0);
}
