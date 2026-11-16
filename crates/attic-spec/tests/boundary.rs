mod common;
use attic_core::config::FastPathScope;
use attic_spec::{
    boundary,
    git::{parse_numstat, parse_status},
};
use common::{Fixture, fixture, git};

#[test]
fn nul_parsers_preserve_rename_state_tabs_newlines_and_reject_truncation() {
    let status = parse_status(b"R  new\tname\n.rs\0old\tname\n.rs\0?? next.rs\0").unwrap();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].orig_path.as_deref(), Some("old\tname\n.rs"));
    let stats = parse_numstat(b"3\t2\t\0old\tname\n.rs\0new\tname\n.rs\0-\t-\tbinary\0").unwrap();
    assert_eq!(stats.len(), 2);
    assert_eq!(stats[0].path, "new\tname\n.rs");
    assert_eq!(stats[0].added, 3);
    assert!(stats[1].binary);
    for bad in [
        b"3\t2\t\0old\0".as_slice(),
        b"3\t2\tmissing-nul",
        b"-\t2\tfile\0",
    ] {
        assert!(parse_numstat(bad).is_err());
    }
    assert!(parse_status(b"R  new\0").is_err());
}

#[test]
fn dirty_baselines_are_content_aware_immutable_and_not_commit_exemptions() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(f.root.join("old1.rs"), "developer1").unwrap();
    std::fs::write(f.root.join("old2.rs"), "developer2").unwrap();
    boundary::capture_baseline(&f.service.store, &f.root, "session").unwrap();
    let cfg = &f.service.config;
    let store = Some(&f.service.store);
    assert!(
        boundary::check_write(
            store,
            &f.root,
            &f.root.join("agent.rs"),
            Some("session"),
            cfg
        )
        .unwrap()
        .allowed
    );
    assert!(
        !boundary::check_write(store, &f.root, &f.root.join("agent.rs"), None, cfg)
            .unwrap()
            .allowed
    );
    std::fs::write(f.root.join("old1.rs"), "changed-again").unwrap();
    boundary::capture_baseline(&f.service.store, &f.root, "session").unwrap();
    assert!(
        !boundary::check_write(
            store,
            &f.root,
            &f.root.join("agent.rs"),
            Some("session"),
            cfg
        )
        .unwrap()
        .allowed
    );
    assert!(
        boundary::check_write(
            store,
            &f.root,
            &f.root.join("docs").join("designs").join("two.md"),
            Some("session"),
            cfg
        )
        .unwrap()
        .allowed
    );
    git(&f.root, &["add", "old1.rs", "old2.rs"]);
    assert!(
        !boundary::check_commit(store, std::slice::from_ref(&f.root), &f.root, cfg)
            .unwrap()
            .allowed
    );
    std::fs::write(f.root.join("binary.dat"), b"\0binary").unwrap();
    assert!(
        !boundary::check_write(
            store,
            &f.root,
            &f.root.join("binary.dat"),
            Some("session"),
            cfg
        )
        .unwrap()
        .allowed
    );
}

#[test]
fn staged_branch_receipts_survive_commit_reopen_and_grace_but_not_changed_blobs() {
    let mut f = Fixture::new(
        "pub fn original() {\n}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture"]);
    git(&f.root, &["update-ref", "refs/heads/baseline", "HEAD"]);
    let doc = f.begun();
    std::fs::write(
        f.root.join("src").join("lib.rs"),
        "pub fn original() {\n    let _x = 1;\n}\n",
    )
    .unwrap();
    f.reindex();
    git(&f.root, &["add", "src/lib.rs"]);
    let completed = f
        .service
        .wave_complete(
            &doc,
            doc.lease_id.as_ref().unwrap(),
            doc.fencing_token,
            1,
            &f.repo.id,
        )
        .unwrap();
    let done = f
        .service
        .verify(
            &completed,
            true,
            std::slice::from_ref(&f.repo),
            &[],
            Some((
                completed.lease_id.as_ref().unwrap(),
                completed.fencing_token,
            )),
            false,
        )
        .unwrap();
    f.service.config.fast_path.scope = FastPathScope::Branch;
    f.service.config.fast_path.base_branch = Some("baseline".into());
    f.service.config.fast_path.commit_grace_hours = 0;
    let check = || {
        boundary::check_commit(
            Some(&f.service.store),
            std::slice::from_ref(&f.root),
            &f.root,
            &f.service.config,
        )
        .unwrap()
    };
    assert!(check().allowed, "{}", check().message);
    git(&f.root, &["commit", "--quiet", "-m", "completed"]);
    f.service
        .store
        .reopen(
            &done,
            "new revision",
            "CLI",
            attic_spec::now_micros().unwrap(),
        )
        .unwrap();
    assert!(check().allowed, "{}", check().message);
    // Different unstaged content cannot revoke approval of the unchanged index.
    std::fs::write(
        f.root.join("src").join("lib.rs"),
        "pub fn original() {\n    unapproved();\n    additional();\n}\n",
    )
    .unwrap();
    assert!(check().allowed);
    git(&f.root, &["add", "src/lib.rs"]);
    assert!(!check().allowed, "{}", check().message);
}

#[test]
fn initial_sha256_commit_scope_does_not_assume_sha1_empty_tree() {
    let dir = fixture();
    git(dir.path(), &["init", "--quiet", "--object-format=sha256"]);
    std::fs::write(dir.path().join("new.rs"), "fn new() {}\n").unwrap();
    git(dir.path(), &["add", "new.rs"]);
    let changes = attic_spec::git::diff(dir.path(), None).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].added, 1);
}

fn completed(f: &Fixture, doc: &attic_spec::state::Document) {
    let doc = f
        .service
        .wave_complete(
            doc,
            doc.lease_id.as_ref().unwrap(),
            doc.fencing_token,
            1,
            &f.repo.id,
        )
        .unwrap();
    let done = f
        .service
        .verify(
            &doc,
            true,
            std::slice::from_ref(&f.repo),
            &[],
            Some((doc.lease_id.as_ref().unwrap(), doc.fencing_token)),
            false,
        )
        .unwrap();
    assert_eq!(done.status, attic_spec::state::Status::Implemented);
}

#[test]
fn staged_grace_receipts_cover_allow_globs_without_exact_file_citations() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs\nALLOW_GLOB repo tests/**",
        true,
    );
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture"]);
    let doc = f.begun();
    std::fs::create_dir_all(f.root.join("tests")).unwrap();
    for name in ["one.rs", "two.rs", "three.rs"] {
        std::fs::write(f.root.join("tests").join(name), "fn approved() {}\n").unwrap();
    }
    git(&f.root, &["add", "tests"]);
    f.reindex();
    completed(&f, &doc);
    let check = || {
        boundary::check_commit(
            Some(&f.service.store),
            std::slice::from_ref(&f.root),
            &f.root,
            &f.service.config,
        )
        .unwrap()
    };
    assert!(check().allowed, "{}", check().message);
    for name in ["one.rs", "two.rs", "three.rs"] {
        std::fs::write(f.root.join("tests").join(name), "fn changed_again() {}\n").unwrap();
    }
    git(&f.root, &["add", "tests"]);
    assert!(!check().allowed, "{}", check().message);
}

#[test]
fn staged_grace_approval_survives_only_unstaged_worktree_changes() {
    let f = Fixture::new(
        "pub fn original() {\n}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    git(&f.root, &["add", "."]);
    git(&f.root, &["commit", "--quiet", "-m", "fixture"]);
    let doc = f.begun();
    let path = f.root.join("src").join("lib.rs");
    std::fs::write(&path, "pub fn original() {\n let _a=1;\n let _b=2;\n}\n").unwrap();
    git(&f.root, &["add", "src/lib.rs"]);
    f.reindex();
    completed(&f, &doc);
    std::fs::write(
        &path,
        "pub fn original() {\n unapproved();\n changed_again();\n}\n",
    )
    .unwrap();
    let check = || {
        boundary::check_commit(
            Some(&f.service.store),
            std::slice::from_ref(&f.root),
            &f.root,
            &f.service.config,
        )
        .unwrap()
    };
    assert!(check().allowed, "{}", check().message);
    git(&f.root, &["add", "src/lib.rs"]);
    assert!(!check().allowed, "{}", check().message);
}
