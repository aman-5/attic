use attic_spec::{
    citations::{self, Kind},
    state::{self, Document, Operation, Status},
};

#[test]
fn grammar_rejects_unsafe_missing_duplicate_and_ambiguous_blocks() {
    let wrap = |line: &str| format!("```attic-citations\n{line}\n```\n");
    let valid = "TARGET_MODIFY attic src/main.rs main";
    assert_eq!(
        citations::parse(wrap(valid).as_bytes()).unwrap()[0].kind,
        Kind::TargetModify
    );
    for bad in [
        "TARGET_MODIFY attic ../outside",
        "TARGET_MODIFY attic /outside",
        "TARGET_MODIFY attic C:/outside",
        "TARGET_MODIFY attic src//main.rs",
        "TARGET_MODIFY attic src/./main.rs",
        "TARGET_MODIFY attic src\\main.rs",
        "TARGET_MODIFY attic src/\0main.rs",
        "TARGET_CREATE_SYMBOL attic src/main.rs",
        "CROSS_REPO_DEPENDENCY a => b",
        "EXISTING_REF a a",
        "ALLOW_GLOB a **/*",
        "TARGET_MODIFY a b c extra",
        "TARGET_MODIFY a",
        "UNKNOWN a b",
        "TARGET_MODIFY a b\nALLOW_GLOB a [invalid",
        "TARGET_MODIFY a b\nALLOW_GLOB a **/* symbol",
    ] {
        assert!(citations::parse(wrap(bad).as_bytes()).is_err(), "{bad}");
    }
    for bad in [
        valid.to_string(),
        format!("```attic-citations\n{valid}"),
        format!("{}{}", wrap(valid), wrap(valid)),
        wrap(&format!("{valid}\n{valid}")),
        format!("````markdown\n{}\n````", wrap(valid)),
    ] {
        assert!(citations::parse(bad.as_bytes()).is_err(), "{bad}");
    }
    assert!(citations::parse(&vec![b'a'; 1_048_577]).is_err());
}

#[test]
fn transition_admission_matrix() {
    use Operation::*;
    use Status::*;
    let statuses = [
        UnderReview,
        Verified,
        Approved,
        Implementing,
        ImplementationInterrupted,
        Implemented,
        Drifted,
        Abandoned,
    ];
    let ops = [
        Register,
        VerifyPass,
        VerifyFail,
        Approve,
        Begin,
        Heartbeat,
        WaveComplete,
        Resume,
        DriftPass,
        DriftFail,
        Reopen,
        Abandon,
    ];
    let allowed = [
        [
            true, true, true, false, false, false, false, false, false, false, true, true,
        ],
        [
            true, true, true, true, false, false, false, false, false, false, false, true,
        ],
        [
            true, true, true, false, true, false, false, false, false, false, true, true,
        ],
        [
            true, false, false, false, false, true, true, false, true, true, true, true,
        ],
        [
            true, false, false, false, false, false, false, true, false, false, true, true,
        ],
        [
            true, false, false, false, false, false, false, false, true, true, true, true,
        ],
        [
            true, false, false, false, false, false, false, false, true, true, true, true,
        ],
        [
            true, false, false, false, false, false, false, false, false, false, false, true,
        ],
    ];
    for (i, s) in statuses.iter().enumerate() {
        for (j, op) in ops.iter().enumerate() {
            assert_eq!(
                state::transition(*s, *op).is_ok(),
                allowed[i][j],
                "{s:?}/{op:?}"
            );
        }
    }
}

#[test]
fn reconciliation_clears_and_fences_every_implementing_exit() {
    let mut doc = Document::new("one", "root", "docs/designs/one.md", "one", 0);
    doc.status = Status::Implementing;
    doc.approved_doc_sha256 = Some("same".into());
    doc.lease_id = Some("lease".into());
    doc.lease_expires_at = Some(100);
    doc.fencing_token = 4;
    for (hash, time, reason) in [
        ("same", 100, "LEASE_EXPIRED"),
        ("changed", 50, "DOC_CHANGED"),
    ] {
        let next = state::reconcile(&doc, hash, time).unwrap();
        assert_eq!(next.status, Status::ImplementationInterrupted);
        assert_eq!(next.status_reason.as_deref(), Some(reason));
        assert!(next.lease_id.is_none() && next.lease_expires_at.is_none());
        assert_eq!(next.fencing_token, 5);
        assert!(next.check_fence("lease", 4).is_err());
    }
    assert_eq!(
        state::reconcile(&doc, "same", 99).unwrap().status,
        Status::Implementing
    );
}
