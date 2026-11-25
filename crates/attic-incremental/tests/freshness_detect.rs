//! Freshness detection scenarios (plan §7) for `reconcile_repository_with`:
//! the metadata fast path, change detection that survives timestamp-
//! preserving tools, and the R2 rule that uncertain state is never deleted.

mod common;

use std::time::Duration;

use attic_discovery::DiscoveryPolicy;
use attic_incremental::{IncrementalError, ReconcileOptions, reconcile_repository_with};
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use common::{Fixture, write_file};

fn reconcile(fx: &Fixture, opts: &ReconcileOptions) -> attic_incremental::ReconcileReport {
    reconcile_repository_with(&fx.pool, fx.root(), &fx.policy(), opts).expect("reconcile")
}

/// Re-run the full index after the racy window so stored capture times are
/// strictly newer than every file's timestamps (fast path eligible).
fn settle(fx: &Fixture) {
    std::thread::sleep(Duration::from_millis(2_200));
    let store = IndexingStore {
        readers: &fx.pool,
        writer: &fx.writer,
    };
    let opts = IndexOptions {
        repository_name: "fixture".into(),
        ..Default::default()
    };
    index_repository(&store, fx.root(), &fx.policy(), &opts).expect("re-index");
}

#[test]
fn unchanged_files_are_proven_by_metadata_without_reading() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
    settle(&fx);
    let r = reconcile(&fx, &ReconcileOptions::default());
    assert!(!r.change_set.has_verified_work());
    assert_eq!(r.files_hashed, 0, "unchanged files must not be re-read");
    assert_eq!(r.files_unchanged_by_metadata, 2);

    // S24: always_hash disables the fast path.
    let r = reconcile(&fx, &ReconcileOptions { always_hash: true });
    assert_eq!(r.files_hashed, 2);
    assert_eq!(r.files_unchanged_by_metadata, 0);
    assert!(!r.change_set.has_verified_work());
}

#[test]
fn recently_written_files_are_always_hashed() {
    // Racy-file rule: files written within the window of the capture are
    // re-hashed even when metadata matches.
    let fx = Fixture::new(&[("a.rs", "fn a() {}")]);
    let r = reconcile(&fx, &ReconcileOptions::default());
    assert_eq!(r.files_hashed, 1);
    assert!(!r.change_set.has_verified_work());
}

#[test]
fn edit_add_and_folder_delete_are_detected() {
    let fx = Fixture::new(&[
        ("a.rs", "fn a() {}"),
        ("dir/x.rs", "fn x() {}"),
        ("dir/sub/y.rs", "fn y() {}"),
    ]);
    settle(&fx);
    write_file(fx.root(), "a.rs", "fn a() { changed() }");
    write_file(fx.root(), "new.rs", "fn new() {}");
    std::fs::remove_dir_all(fx.root().join("dir")).unwrap();

    let r = reconcile(&fx, &ReconcileOptions::default());
    let cs = &r.change_set;
    assert!(cs.upserts.contains(&"a.rs".to_string()));
    assert!(cs.upserts.contains(&"new.rs".to_string()));
    let mut deletes = cs.deletes.clone();
    deletes.sort();
    assert_eq!(deletes, vec!["dir/sub/y.rs", "dir/x.rs"]);
    assert!(cs.uncertain.is_empty());
}

#[test]
fn in_place_rewrite_preserving_size_and_mtime_is_detected() {
    // S31: copy tools restore the source's last-write time.
    let fx = Fixture::new(&[("a.rs", "fn aaaa() {}")]);
    settle(&fx);
    let path = fx.root().join("a.rs");
    let before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, "fn bbbb() {}").unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(before)
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        before
    );

    let r = reconcile(&fx, &ReconcileOptions::default());
    assert_eq!(r.change_set.upserts, vec!["a.rs".to_string()]);
}

#[test]
fn plain_folder_gets_the_same_detection() {
    // S5: non-Git policy, same mechanism.
    let fx = Fixture::new(&[("notes/a.txt", "alpha")]);
    let policy = DiscoveryPolicy::default_non_git();
    write_file(fx.root(), "notes/a.txt", "alpha beta");
    write_file(fx.root(), "notes/b.txt", "gamma");
    let r = reconcile_repository_with(&fx.pool, fx.root(), &policy, &ReconcileOptions::default())
        .unwrap();
    let mut upserts = r.change_set.upserts.clone();
    upserts.sort();
    assert_eq!(upserts, vec!["notes/a.txt", "notes/b.txt"]);
}

#[test]
fn missing_or_replaced_empty_root_is_unavailable_not_deleted() {
    // S27: an unmounted / replaced root must never wipe the index.
    let fx = Fixture::new(&[("a.rs", "fn a() {}")]);
    let root = fx.root().to_path_buf();
    std::fs::remove_dir_all(&root).unwrap();
    let err =
        reconcile_repository_with(&fx.pool, &root, &fx.policy(), &Default::default()).unwrap_err();
    assert!(
        matches!(err, IncrementalError::SourceUnavailable(_)),
        "{err}"
    );

    // Same path, different (empty) directory object.
    std::fs::create_dir_all(&root).unwrap();
    let err =
        reconcile_repository_with(&fx.pool, &root, &fx.policy(), &Default::default()).unwrap_err();
    assert!(
        matches!(err, IncrementalError::SourceUnavailable(_)),
        "{err}"
    );
    assert!(fx.occurrence("a.rs").is_some());
}

#[test]
fn emptied_root_with_same_identity_deletes_normally() {
    let fx = Fixture::new(&[("a.rs", "fn a() {}")]);
    std::fs::remove_file(fx.root().join("a.rs")).unwrap();
    let r = reconcile(&fx, &ReconcileOptions::default());
    assert_eq!(r.change_set.deletes, vec!["a.rs".to_string()]);
}

#[test]
fn recloned_root_is_fully_hashed() {
    // S35: delete + re-create at the same path → new identity → full hash.
    let fx = Fixture::new(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
    settle(&fx);
    let root = fx.root().to_path_buf();
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    write_file(&root, "a.rs", "fn a() {}");
    write_file(&root, "b.rs", "fn b() { changed() }");
    let r = reconcile(&fx, &ReconcileOptions::default());
    assert!(r.root_identity_changed);
    assert_eq!(r.files_hashed, 2, "fast path must be off after a re-clone");
    assert_eq!(r.change_set.upserts, vec!["b.rs".to_string()]);
    assert!(r.change_set.deletes.is_empty());
}

#[cfg(windows)]
#[test]
fn locked_file_is_uncertain_not_deleted() {
    // S16: an exclusively-locked file cannot be read.
    use std::os::windows::fs::OpenOptionsExt;
    let fx = Fixture::new(&[("a.rs", "fn a() {}"), ("b.rs", "fn b() {}")]);
    write_file(fx.root(), "a.rs", "fn a() { changed() }");
    let _lock = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(fx.root().join("a.rs"))
        .unwrap();
    let r = reconcile(&fx, &ReconcileOptions::default());
    assert_eq!(r.change_set.uncertain, vec!["a.rs".to_string()]);
    assert!(r.change_set.deletes.is_empty());
    assert!(!r.change_set.upserts.contains(&"a.rs".to_string()));
}

#[cfg(windows)]
#[test]
fn unreadable_subdirectory_is_uncertain_not_deleted() {
    // S34: deny listing a folder; its indexed files must not be deleted.
    let fx = Fixture::new(&[("a.rs", "fn a() {}"), ("locked/x.rs", "fn x() {}")]);
    let dir = fx.root().join("locked");
    let deny = |action: &str| {
        std::process::Command::new("icacls")
            .arg(&dir)
            .args([action, "*S-1-1-0:(RD)"])
            .output()
            .expect("icacls")
    };
    let out = deny("/deny");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let r = reconcile_repository_with(&fx.pool, fx.root(), &fx.policy(), &Default::default());
    let restored = std::process::Command::new("icacls")
        .arg(&dir)
        .args(["/remove:d", "*S-1-1-0"])
        .output()
        .expect("icacls restore");
    assert!(restored.status.success());
    let r = r.unwrap();
    assert!(
        r.change_set.deletes.is_empty(),
        "files under an unreadable folder were deleted: {:?}",
        r.change_set.deletes
    );
    assert_eq!(r.change_set.uncertain, vec!["locked/x.rs".to_string()]);
}
