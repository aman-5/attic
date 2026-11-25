//! Freshness-gate scenarios (plan §7) at library level: `ensure_current`
//! brings the index to the saved files before an answer, labels the code,
//! and never lets an older publication overwrite a newer one.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use attic_discovery::DiscoveryPolicy;
use attic_incremental::{
    ExpectedSource, GateConfig, IncrementalError, check_expected, ensure_current,
};
use attic_indexing::{IndexingStore, ScopedChanges, index_changes_fenced};
use common::{Fixture, git_init_isolated, write_file};

fn gate(fx: &Fixture) -> attic_incremental::GateOutcome {
    ensure_current(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        &GateConfig::default(),
    )
    .expect("ensure_current")
}

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn answer_right_after_bulk_change_sees_new_content_without_scheduler() {
    // S1/S2: a branch switch or pull rewrites many files; the very next
    // answer must see them — no watcher, no scheduler run in between.
    let fx = Fixture::new(&[
        ("a.rs", "fn old_alpha_token() {}"),
        ("b.rs", "fn old_beta_token() {}"),
        ("gone.rs", "fn old_gone_token() {}"),
    ]);
    let before = gate(&fx).version;
    write_file(fx.root(), "a.rs", "fn new_alpha_token() {}");
    write_file(fx.root(), "b.rs", "fn new_beta_token() {}");
    write_file(fx.root(), "c.rs", "fn new_gamma_token() {}");
    std::fs::remove_file(fx.root().join("gone.rs")).unwrap();

    let out = gate(&fx);
    assert_eq!(out.files_reindexed, 4);
    assert_ne!(out.version, before);
    assert!(fx.search("old_alpha_token").is_empty());
    assert!(fx.search("old_gone_token").is_empty());
    assert_eq!(fx.search("new_alpha_token").len(), 1);
    assert_eq!(fx.search("new_gamma_token").len(), 1);
    assert!(out.uncertain.is_empty());

    // Converged: a second check changes nothing and keeps the version.
    let again = gate(&fx);
    assert_eq!(again.files_reindexed, 0);
    assert_eq!(again.version, out.version);
    assert_eq!(
        again.source_label.content_fingerprint,
        out.source_label.content_fingerprint
    );
}

#[test]
fn git_label_and_branch_assertion() {
    // S21: answering about `prod` while `main` is checked out is refused.
    let fx = Fixture::new(&[("a.rs", "fn a() {}")]);
    git_init_isolated(fx.root());
    git(fx.root(), &["checkout", "-q", "-b", "main"]);
    git(fx.root(), &["add", "."]);
    git(fx.root(), &["commit", "-q", "-m", "init"]);
    let out = gate(&fx);
    assert_eq!(out.source_label.kind, "git");
    assert_eq!(out.source_label.branch.as_deref(), Some("main"));
    let head = out.source_label.commit.clone().expect("commit");
    assert!(
        check_expected(
            &out.source_label,
            &ExpectedSource {
                branch: Some("main".into()),
                commit: Some(head[..7].into()),
            }
        )
        .is_ok()
    );
    let err = check_expected(
        &out.source_label,
        &ExpectedSource {
            branch: Some("prod".into()),
            commit: None,
        },
    )
    .unwrap_err();
    assert!(err.contains("prod") && err.contains("main"), "{err}");

    git(fx.root(), &["checkout", "-q", "-b", "prod"]);
    write_file(fx.root(), "a.rs", "fn prod_only() {}");
    let out = gate(&fx);
    assert_eq!(out.source_label.branch.as_deref(), Some("prod"));
    assert_eq!(fx.search("prod_only").len(), 1);
}

#[test]
fn plain_folder_is_labelled_folder() {
    let fx = Fixture::new(&[("notes.txt", "alpha")]);
    let out = ensure_current(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &DiscoveryPolicy::default_non_git(),
        &GateConfig::default(),
    )
    .unwrap();
    assert_eq!(out.source_label.kind, "folder");
    assert_eq!(out.source_label.branch, None);
    assert!(!out.source_label.content_fingerprint.is_empty());
}

#[test]
fn older_background_publication_is_rejected_after_gate_publish() {
    // S26/S32: a run that started before the gate published must not
    // overwrite the newer state.
    let fx = Fixture::new(&[("a.rs", "fn first_version() {}")]);
    let stale = gate(&fx).version;
    write_file(fx.root(), "a.rs", "fn second_version() {}");
    gate(&fx);
    write_file(fx.root(), "a.rs", "fn third_version() {}");
    let store = IndexingStore {
        readers: &fx.pool,
        writer: &fx.writer,
    };
    let res = index_changes_fenced(
        &store,
        fx.root(),
        &fx.policy(),
        &Default::default(),
        &ScopedChanges {
            upserts: vec!["a.rs".into()],
            ..Default::default()
        },
        Some(stale),
    )
    .unwrap();
    assert!(res.rejected);
    assert_eq!(fx.search("second_version").len(), 1);
    assert!(fx.search("third_version").is_empty());
    // The gate picks the newest content up.
    gate(&fx);
    assert_eq!(fx.search("third_version").len(), 1);
}

#[test]
fn store_that_keeps_moving_yields_source_changing() {
    // S7: when the index keeps changing under the check, retries are
    // bounded and the gate reports SOURCE_CHANGING instead of answering.
    let fx = Fixture::new(&[("a.rs", "fn a() {}")]);
    write_file(fx.root(), "a.rs", "fn a2() {}");
    let writer = fx.writer.clone();
    let counter = Arc::new(AtomicUsize::new(0));
    let hook_counter = Arc::clone(&counter);
    let res = ensure_current(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        &GateConfig {
            retry_backoff: vec![std::time::Duration::ZERO; 2],
            // Another writer commits between the check and every publish.
            before_publish: Some(Arc::new(move || {
                let i = hook_counter.fetch_add(1, Ordering::SeqCst);
                writer
                    .send(move |c| {
                        c.execute(
                            "INSERT INTO core_invalidation_records
                                (id, artifact_type, artifact_id, reason, invalidated_at)
                             VALUES (?1, 'FILE_OCCURRENCE', 'x', 'EXPLICIT', 0)",
                            [format!("concurrent-{i}")],
                        )?;
                        Ok(())
                    })
                    .unwrap();
            })),
            ..Default::default()
        },
    );
    assert!(
        matches!(res, Err(IncrementalError::SourceChanging(_))),
        "{res:?}"
    );
    assert_eq!(counter.load(Ordering::SeqCst), 3, "1 attempt + 2 retries");
    assert!(fx.search("a2").is_empty(), "nothing published while racing");
    // Once the other writer stops, the next check succeeds.
    gate(&fx);
    assert_eq!(fx.search("a2").len(), 1);
}

#[test]
fn two_publications_fenced_on_one_version_in_one_batch_cannot_both_apply() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
    write_file(fx.root(), "a.rs", "fn batch_a() {}");
    write_file(fx.root(), "b.rs", "fn batch_b() {}");
    let v = gate(&fx).version;
    write_file(fx.root(), "a.rs", "fn batch_a2() {}");
    write_file(fx.root(), "b.rs", "fn batch_b2() {}");
    let run = |path: &'static str| {
        let fx = &fx;
        let v = v.clone();
        move || {
            let store = IndexingStore {
                readers: &fx.pool,
                writer: &fx.writer,
            };
            index_changes_fenced(
                &store,
                fx.root(),
                &fx.policy(),
                &Default::default(),
                &ScopedChanges {
                    upserts: vec![path.into()],
                    ..Default::default()
                },
                Some(v),
            )
            .unwrap()
            .rejected
        }
    };
    let (ra, rb) = std::thread::scope(|s| {
        let a = s.spawn(run("a.rs"));
        let b = s.spawn(run("b.rs"));
        (a.join().unwrap(), b.join().unwrap())
    });
    assert!(
        ra ^ rb,
        "exactly one fenced publication may apply (a={ra}, b={rb})"
    );
}

#[test]
fn concurrent_checks_serialize_and_agree() {
    let fx = Arc::new(Fixture::new(&[("a.rs", "fn a() {}")]));
    write_file(fx.root(), "a.rs", "fn concurrent_token() {}");
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let fx = Arc::clone(&fx);
            std::thread::spawn(move || gate(&fx))
        })
        .collect();
    let outs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let reindexed: usize = outs.iter().map(|o| o.files_reindexed).sum();
    assert_eq!(reindexed, 1, "exactly one caller does the work");
    let last = gate(&fx).version;
    assert!(outs.iter().all(|o| o.version == last));
    assert_eq!(fx.search("concurrent_token").len(), 1);
}
