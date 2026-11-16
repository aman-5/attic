mod common;
use attic_spec::{
    Result,
    service::ApprovalPrompt,
    state::{Document, Status},
    verifier::{self, Run},
};
use common::{Fixture, Prompt};

#[test]
fn complete_fenced_flow_pins_approval_and_resumes_with_new_fence() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    let doc = f.begun();
    let lease = doc.lease_id.clone().unwrap();
    let token = doc.fencing_token;
    assert_eq!(doc.status, Status::Implementing);
    assert_eq!(f.service.waves(&doc).unwrap()[0].status, "IN_PROGRESS");
    assert_eq!(
        f.service
            .verify(
                &doc,
                true,
                std::slice::from_ref(&f.repo),
                &[],
                Some((&lease, token)),
                false
            )
            .unwrap_err()
            .code,
        "ERR_WAVES_INCOMPLETE"
    );
    let heartbeat = f.service.heartbeat(&doc, &lease, token).unwrap();
    assert_eq!(
        f.service.heartbeat(&doc, &lease, token).unwrap_err().code,
        "ERR_STATE_CHANGED_DURING_APPROVAL"
    );
    f.advance(1_000_000_000);
    assert_eq!(
        f.service
            .heartbeat(&heartbeat, &lease, token)
            .unwrap_err()
            .code,
        "ERR_LEASE_EXPIRED"
    );
    let interrupted = f.service.resolve("one", None).unwrap();
    assert_eq!(interrupted.status, Status::ImplementationInterrupted);
    let resumed = f.service.resume(&interrupted).unwrap();
    assert!(resumed.fencing_token > token);
    assert!(
        f.service
            .wave_complete(&resumed, &lease, token, 1, &f.repo.id)
            .is_err()
    );
    let current = f
        .service
        .wave_complete(
            &resumed,
            resumed.lease_id.as_ref().unwrap(),
            resumed.fencing_token,
            1,
            &f.repo.id,
        )
        .unwrap();
    let done = f
        .service
        .verify(
            &current,
            true,
            std::slice::from_ref(&f.repo),
            &[],
            Some((current.lease_id.as_ref().unwrap(), current.fencing_token)),
            false,
        )
        .unwrap();
    assert_eq!(done.status, Status::Implemented);
    assert!(done.lease_id.is_none());
    assert!(done.fencing_token > current.fencing_token);
    let approved_run = done.approved_run_id.clone().unwrap();
    let reopened = f
        .service
        .store
        .reopen(&done, "new scope", "CLI", f.clock.now().unwrap())
        .unwrap();
    assert_eq!(reopened.revision, 2);
    assert!(f.service.waves(&reopened).unwrap().is_empty());
    assert_eq!(f.service.run(&approved_run).unwrap().kind, "PRE_CODE");
}

#[test]
fn approval_cancellation_non_tty_and_prompt_races_do_not_approve() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    let doc = f.verified();
    assert_eq!(
        f.service
            .approve(&doc, &mut Prompt(false))
            .unwrap_err()
            .code,
        "ERR_APPROVAL_CANCELLED"
    );
    struct NonTty;
    impl ApprovalPrompt for NonTty {
        fn is_interactive(&self) -> bool {
            false
        }
        fn confirm(&mut self, _: &Document, _: &Run, _: &str) -> Result<bool> {
            panic!("must not prompt")
        }
    }
    assert_eq!(
        f.service.approve(&doc, &mut NonTty).unwrap_err().code,
        "ERR_APPROVAL_REQUIRES_TTY"
    );
    struct Mutate<'a> {
        f: &'a Fixture,
        doc: &'a Document,
    }
    impl ApprovalPrompt for Mutate<'_> {
        fn is_interactive(&self) -> bool {
            true
        }
        fn confirm(&mut self, _: &Document, _: &Run, _: &str) -> Result<bool> {
            self.f.service.store.abandon(
                self.doc,
                "concurrent abandon",
                "CLI",
                attic_spec::now_micros()?,
            )?;
            Ok(true)
        }
    }
    assert_eq!(
        f.service
            .approve(&doc, &mut Mutate { f: &f, doc: &doc })
            .unwrap_err()
            .code,
        "ERR_STATE_CHANGED_DURING_APPROVAL"
    );
    assert_eq!(
        f.service.resolve("one", None).unwrap().status,
        Status::Abandoned
    );
    let doc = f.verified();
    struct Edit(std::path::PathBuf);
    impl ApprovalPrompt for Edit {
        fn is_interactive(&self) -> bool {
            true
        }
        fn confirm(&mut self, _: &Document, _: &Run, _: &str) -> Result<bool> {
            std::fs::write(&self.0, "changed")?;
            Ok(true)
        }
    }
    assert_eq!(
        f.service
            .approve(
                &doc,
                &mut Edit(f.root.join("docs").join("designs").join("one.md"))
            )
            .unwrap_err()
            .code,
        "ERR_APPROVAL_STALE"
    );
}

#[test]
fn approval_is_revoked_before_stale_begin_refuses_and_failed_drift_clears_lease() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs original",
        true,
    );
    let doc = f.approved();
    std::fs::write(f.root.join("src").join("lib.rs"), "pub fn changed() {}\n").unwrap();
    f.reindex();
    assert_eq!(
        f.service
            .verify(&doc, false, std::slice::from_ref(&f.repo), &[], None, true)
            .unwrap_err()
            .code,
        "ERR_APPROVAL_STALE"
    );
    let stale = f.service.resolve("one", None).unwrap();
    assert_eq!(stale.status, Status::UnderReview);
    assert!(stale.approved_run_id.is_none());
    assert!(
        f.service
            .run(stale.latest_run_id.as_ref().unwrap())
            .unwrap()
            .citations
            .iter()
            .any(|c| c.error)
    );
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs original",
        true,
    );
    let doc = f.begun();
    let doc = f
        .service
        .wave_complete(
            &doc,
            doc.lease_id.as_ref().unwrap(),
            doc.fencing_token,
            1,
            &f.repo.id,
        )
        .unwrap();
    std::fs::write(f.root.join("src").join("lib.rs"), "pub fn changed() {}\n").unwrap();
    f.reindex();
    let failed = f
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
    assert_eq!(failed.status, Status::Drifted);
    assert!(failed.lease_id.is_none());
    assert!(failed.fencing_token > doc.fencing_token);
    assert_eq!(
        verifier::disk_hash(&f.root.join("src").join("lib.rs"))
            .unwrap()
            .unwrap()
            .len(),
        64
    );
}

use attic_spec::service::Clock;

#[test]
fn lease_expiring_after_admission_is_persistently_interrupted_not_renewed() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    struct StepClock {
        calls: AtomicUsize,
        expiry: i64,
    }
    impl Clock for StepClock {
        fn now(&self) -> Result<i64> {
            Ok(if self.calls.fetch_add(1, Ordering::SeqCst) < 2 {
                self.expiry - 1
            } else {
                self.expiry + 1
            })
        }
    }
    let mut f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    let doc = f.begun();
    f.service.clock = Arc::new(StepClock {
        calls: AtomicUsize::new(0),
        expiry: doc.lease_expires_at.unwrap(),
    });
    assert_eq!(
        f.service
            .heartbeat(&doc, doc.lease_id.as_ref().unwrap(), doc.fencing_token)
            .unwrap_err()
            .code,
        "ERR_LEASE_EXPIRED"
    );
    let actual = f.service.resolve("one", None).unwrap();
    assert_eq!(actual.status, Status::ImplementationInterrupted);
    assert!(actual.lease_id.is_none());
    assert!(actual.fencing_token > doc.fencing_token);
}

#[test]
fn deleted_document_persists_expiry_and_revocation_before_refusing_worker_then_abandons() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    let doc = f.begun();
    let approved_run = doc.approved_run_id.clone().unwrap();
    std::fs::remove_file(f.root.join("docs").join("designs").join("one.md")).unwrap();
    f.advance(1_000_000_000);
    assert_eq!(
        f.service
            .heartbeat(&doc, doc.lease_id.as_ref().unwrap(), doc.fencing_token)
            .unwrap_err()
            .code,
        "ERR_LEASE_EXPIRED"
    );
    let current = f.service.resolve("one", None).unwrap();
    assert_eq!(current.status, Status::ImplementationInterrupted);
    assert!(current.lease_id.is_none());
    assert!(current.lease_expires_at.is_none());
    assert!(current.fencing_token > doc.fencing_token);
    assert!(current.approved_run_id.is_none());
    assert!(current.verified_doc_sha256.is_none());
    let (active, total): (i64, i64) = f
        .service
        .store
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(CASE WHEN voided_at IS NULL THEN 1 END),COUNT(*) FROM spec_approvals",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!((active, total), (0, 1));
    assert_eq!(f.service.run(&approved_run).unwrap().kind, "PRE_CODE");
    let current = f.service.reconciled(&current).unwrap();
    let abandoned = f
        .service
        .store
        .abandon(&current, "document deleted", "CLI", f.clock.now().unwrap())
        .unwrap();
    f.service.invalidate_packs(&abandoned).unwrap();
    assert_eq!(f.service.status(&abandoned).unwrap()["status"], "ABANDONED");
}

#[test]
fn renamed_document_revokes_approval_and_allows_reopen_without_old_bytes() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs",
        true,
    );
    let doc = f.approved();
    let path = f.root.join("docs").join("designs").join("one.md");
    std::fs::rename(&path, path.with_file_name("renamed.md")).unwrap();
    assert!(
        f.service
            .verify(&doc, false, std::slice::from_ref(&f.repo), &[], None, true)
            .is_err()
    );
    let current = f.service.resolve("one", None).unwrap();
    assert_eq!(current.status, Status::UnderReview);
    assert!(current.approved_run_id.is_none());
    let reopened = f
        .service
        .store
        .reopen(&current, "document renamed", "CLI", f.clock.now().unwrap())
        .unwrap();
    f.service.invalidate_packs(&reopened).unwrap();
    assert_eq!(reopened.revision, 2);
    assert_eq!(
        f.service.status(&reopened).unwrap()["status"],
        "UNDER_REVIEW"
    );
}
