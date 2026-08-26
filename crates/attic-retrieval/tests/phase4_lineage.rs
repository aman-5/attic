//! Phase 4 hardening proofs — lineage preservation (#1):
//! live-source verification NEVER rewrites indexed freshness/revision/
//! generation; truth is expressed via verification state +
//! `live_source_verified`, which sufficiency may accept under CURRENT_ONLY.

mod common;

use attic_core::FreshnessState;
use attic_evidence::{AuthorityLevel, Evidence, VerificationStatus};
use attic_retrieval::budget::BudgetAccountant;
use attic_retrieval::mode::{AnswerMode as AM, AnswerModePolicy};
use attic_retrieval::verify::{self, VerifyOutcome};
use common::Fixture;

#[test]
fn verification_preserves_indexed_freshness_revision_and_generation() {
    let fx = Fixture::bootstrap();
    fx.set_path_freshness("services/pay.py", "STALE");

    let lineage = |fx: &Fixture| -> (String, Option<String>, String) {
        fx.pool
            .with_reader(|c| {
                c.query_row(
                    "SELECT source_revision_id, index_generation_id, freshness_state
                       FROM core_file_occurrences WHERE path='services/pay.py'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(attic_storage::StorageError::from)
            })
            .unwrap()
    };

    let before = lineage(&fx);
    assert_eq!(before.2, "STALE", "setup: artifact must start stale");

    // Recovery path: CURRENT_ONLY contract satisfied via live verification.
    let out = fx.ask("Where is process_payment defined?", AM::Normal);
    assert_eq!(out.result.as_str(), "SUCCESS");
    assert!(
        out.plan
            .steps
            .iter()
            .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER"),
        "recovery must be observable through a verifier step"
    );

    let after = lineage(&fx);
    assert_eq!(after.0, before.0, "SourceRevision lineage altered");
    assert_eq!(after.1, before.1, "IndexGeneration lineage altered");
    assert_eq!(
        after.2, "STALE",
        "index freshness falsified by verification"
    );
}

#[test]
fn verify_outcome_sets_flag_without_touching_freshness_or_provenance() {
    let fx = Fixture::bootstrap();
    let root = fx.root.canonicalize().unwrap();

    let mut ev = Evidence::new("ev-lineage", "00000000-0000-0000-0000-000000000000");
    ev.path = "services/pay.py".to_owned();
    ev.source_span = Some(attic_core::SourceSpan::new(0, 0, 20, 0));
    ev.snippet = Some("process_payment(amount_cents: int".to_owned());
    ev.authority = AuthorityLevel::Implementation;
    ev.freshness_state = FreshnessState::Stale; // indexed artifact is stale
    ev.source_revision_id = Some("revision-A".to_owned());
    ev.index_generation_id = Some("generation-B".to_owned());

    let mut policy = AnswerModePolicy::for_mode(AM::Normal);
    policy.max_fs_files = 3;
    policy.max_fs_bytes = 1024 * 1024;
    policy.validate().unwrap();
    let mut budget = BudgetAccountant::new(&policy);

    let outcome = verify::verify_evidence(&mut ev, &root, &policy, &mut budget).expect("verify");

    assert_eq!(outcome, VerifyOutcome::VerifiedCurrent);
    assert!(ev.live_source_verified, "live truth recorded explicitly");
    assert_eq!(ev.verification_state, VerificationStatus::Verified);
    // Lineage untouched:
    assert_eq!(ev.freshness_state, FreshnessState::Stale);
    assert_eq!(ev.source_revision_id.as_deref(), Some("revision-A"));
    assert_eq!(ev.index_generation_id.as_deref(), Some("generation-B"));
}

#[test]
fn sufficiency_accepts_live_verified_evidence_only_through_the_flag() {
    let fx = Fixture::bootstrap();
    fx.set_path_freshness("services/pay.py", "PENDING_REFRESH");
    let out = fx.ask("Where is process_payment defined?", AM::Normal);

    if out.result.as_str() == "SUCCESS" {
        // Acceptance must trace to verification, not stale rows counted
        // as current.
        assert!(
            out.plan
                .steps
                .iter()
                .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER")
        );
    } else {
        assert_ne!(out.confidence.as_str(), "HIGH");
    }
}
