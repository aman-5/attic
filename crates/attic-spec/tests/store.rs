use attic_spec::{
    state::Document,
    store::{self, SpecStore},
};
use rusqlite::{Connection, params};

fn fixture() -> tempfile::TempDir {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("egm-tests");
    std::fs::create_dir_all(&dir).unwrap();
    tempfile::tempdir_in(dir).unwrap()
}

#[test]
fn fresh_reopen_read_only_and_foreign_format_safety() {
    let dir = fixture();
    let path = dir.path().join("spec_state.db");
    assert!(SpecStore::open_read_only(&path).unwrap().is_none());
    assert!(!path.exists());
    let store = SpecStore::open(&path).unwrap();
    let doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
    store.write(|tx| store::insert(tx, &doc)).unwrap();
    drop(store);
    assert_eq!(
        SpecStore::open(&path).unwrap().documents().unwrap().len(),
        1
    );
    assert_eq!(
        SpecStore::open_read_only(&path)
            .unwrap()
            .unwrap()
            .documents()
            .unwrap()[0]
            .spec_id,
        "one"
    );
    let unknown = dir.path().join("unknown.db");
    Connection::open(&unknown)
        .unwrap()
        .execute_batch("CREATE TABLE unrelated(x);")
        .unwrap();
    let before = std::fs::read(&unknown).unwrap();
    assert_eq!(
        SpecStore::open(&unknown).err().unwrap().code,
        "ERR_SPEC_DB_UNRECOGNIZED"
    );
    assert_eq!(std::fs::read(&unknown).unwrap(), before);
    let newer = dir.path().join("newer.db");
    Connection::open(&newer)
        .unwrap()
        .execute_batch("PRAGMA user_version=99;")
        .unwrap();
    let before = std::fs::read(&newer).unwrap();
    assert_eq!(
        SpecStore::open(&newer).err().unwrap().code,
        "ERR_SPEC_DB_TOO_NEW"
    );
    assert_eq!(std::fs::read(&newer).unwrap(), before);
    let corrupt = dir.path().join("corrupt.db");
    std::fs::write(&corrupt, b"not sqlite").unwrap();
    assert!(SpecStore::open(&corrupt).is_err());
    assert_eq!(std::fs::read(corrupt).unwrap(), b"not sqlite");
}

#[test]
fn compare_and_swap_and_lease_constraints() {
    let dir = fixture();
    let store = SpecStore::open(&dir.path().join("spec.db")).unwrap();
    let doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
    store.write(|tx| store::insert(tx, &doc)).unwrap();
    let mut next = doc.clone();
    next.title = "new".into();
    store
        .write(|tx| {
            store::save(
                tx,
                &doc,
                &mut next,
                "test",
                "MCP",
                &serde_json::json!({}),
                2,
            )
        })
        .unwrap();
    assert!(
        store
            .write(|tx| store::save(
                tx,
                &doc,
                &mut doc.clone(),
                "test",
                "MCP",
                &serde_json::json!({}),
                3
            ))
            .is_err()
    );
    store
        .read(|conn| {
            assert!(
                conn.execute(
                    "UPDATE spec_documents SET lease_id='invalid' WHERE spec_uid=?1",
                    [&doc.spec_uid]
                )
                .is_err()
            );
            assert!(
                conn.execute(
                    "UPDATE spec_documents SET status='IMPLEMENTING' WHERE spec_uid=?1",
                    [&doc.spec_uid]
                )
                .is_err()
            );
            assert!(conn.execute("DELETE FROM spec_events", []).is_err());
            Ok(())
        })
        .unwrap();
    assert_eq!(store.documents().unwrap()[0].title, "new");
}

#[test]
fn voided_approval_history_pins_runs_and_cannot_rebind() {
    let dir = fixture();
    let store = SpecStore::open(&dir.path().join("spec.db")).unwrap();
    let doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
    store.write(|tx| {
        store::insert(tx,&doc)?;
        for i in 0..4 {
            tx.execute(            "INSERT INTO spec_verification_runs(run_id,spec_uid,spec_revision,run_kind,
                doc_sha256,citation_fingerprint,revision_set_hash,passed,checked_at)
             VALUES(?1,?2,1,'PRE_CODE','doc','fp','audit',1,?3)",params![format!("r{i}"),doc.spec_uid,i])?;
            tx.execute(            "INSERT INTO spec_citations(run_id,ordinal,citation_kind,verification_tier)
             VALUES(?1,0,'TARGET_MODIFY','FILE_ONLY_VERIFIED')",[format!("r{i}")])?;
        }
        tx.execute("INSERT INTO spec_approvals VALUES('approval',?1,1,'r0','doc','fp','CLI_TTY_CHALLENGE',NULL,2,NULL,NULL)",[&doc.spec_uid])?;
        assert!(tx.execute("UPDATE spec_approvals SET run_id='r1',voided_at=3,void_reason='rebind'",[]).is_err());
        store::void_approval(tx,&doc.spec_uid,"reopen",3)?;
        assert!(tx.execute("UPDATE spec_approvals SET voided_at=NULL,void_reason=NULL",[]).is_err());
        assert!(tx.execute("DELETE FROM spec_approvals",[]).is_err());
        store::retain_runs(tx,&doc,1)?;
        let count:i64=tx.query_row("SELECT COUNT(*) FROM spec_verification_runs",[],|r|r.get(0))?;
        assert_eq!(count,2);
        let count:i64=tx.query_row("SELECT COUNT(*) FROM spec_citations",[],|r|r.get(0))?;
        assert_eq!(count,2);
        Ok(())
    }).unwrap();
}

#[test]
fn concurrent_open_and_short_writes_serialize() {
    let dir = fixture();
    let path = dir.path().join("spec.db");
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || {
                let store = SpecStore::open(&path).unwrap();
                let doc = Document::new(
                    &format!("s{i}"),
                    "root",
                    &format!("docs/designs/s{i}.md"),
                    "title",
                    1,
                );
                store.write(|tx| store::insert(tx, &doc)).unwrap();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(
        SpecStore::open(&path).unwrap().documents().unwrap().len(),
        4
    );
}

#[test]
fn rebuilding_catalog_cannot_remove_authoritative_state() {
    let dir = fixture();
    let catalog = dir.path().join("catalog.db");
    let store = SpecStore::open(&SpecStore::path_for_catalog(&catalog)).unwrap();
    let doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
    store.write(|tx| store::insert(tx, &doc)).unwrap();
    let derived = Connection::open(catalog).unwrap();
    attic_storage::migration::run_migrations(&derived).unwrap();
    derived
        .execute_batch("DROP TABLE core_schema_migrations;")
        .unwrap();
    attic_storage::migration::run_migrations(&derived).unwrap();
    assert_eq!(store.documents().unwrap()[0].spec_uid, doc.spec_uid);
}

#[test]
fn reconciliation_remains_committed_after_followup_refusal_and_reopen_supersedes_only_current_revision()
 {
    use attic_spec::state::Status;
    let dir = fixture();
    let store = SpecStore::open(&dir.path().join("spec.db")).unwrap();
    let mut doc = Document::new("one", "root", "docs/designs/one.md", "one", 1);
    doc.status = Status::Implementing;
    doc.approved_doc_sha256 = Some("same".into());
    doc.lease_id = Some("lease".into());
    doc.lease_expires_at = Some(10);
    store
        .write(|tx| {
            store::insert(tx, &doc)?;
            for (rev, status) in [(1, "IN_PROGRESS"), (2, "PENDING")] {
                tx.execute(
                    "INSERT INTO spec_execution_waves(spec_uid,spec_revision,wave_number,
                       repository_id,ordering_basis,allowed_paths_json,wave_status)
                     VALUES(?1,?2,1,'repo','UNLINKED_SINGLE_WAVE','[]',?3)",
                    params![doc.spec_uid, rev, status],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let reconciled = store.reconcile(&doc, "same", 11).unwrap();
    assert!(reconciled.check_fence("lease", 0).is_err());
    assert!(store.reopen(&reconciled, "", "MCP", 12).is_err());
    assert_eq!(
        store.documents().unwrap()[0].status,
        Status::ImplementationInterrupted
    );
    let reopened = store
        .reopen(&reconciled, "change scope", "MCP", 12)
        .unwrap();
    assert_eq!(reopened.revision, 2);
    assert!(reopened.lease_id.is_none() && reopened.lease_expires_at.is_none());
    assert!(reopened.approved_doc_sha256.is_none());
    assert_eq!(reopened.fencing_token, 2);
    store
        .read(|conn| {
            assert_eq!(
                conn.query_row(
                    "SELECT wave_status FROM spec_execution_waves WHERE spec_revision=1",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "SUPERSEDED"
            );
            assert_eq!(
                conn.query_row(
                    "SELECT wave_status FROM spec_execution_waves WHERE spec_revision=2",
                    [],
                    |r| r.get::<_, String>(0)
                )?,
                "PENDING"
            );
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .reopen(&reconciled, "stale worker", "MCP", 13)
            .is_err()
    );
    assert_eq!(store.documents().unwrap()[0].revision, 2);
    let abandoned = store.abandon(&reopened, "cancel", "CLI", 14).unwrap();
    assert_eq!(abandoned.status, Status::Abandoned);
    let registered = store.reregister(&abandoned, "new attempt", 15).unwrap();
    assert_eq!(registered.status, Status::UnderReview);
    assert_eq!(registered.revision, 3);
    assert!(registered.latest_run_id.is_none());
}
