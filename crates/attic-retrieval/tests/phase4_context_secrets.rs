//! Phase 4 hardening proofs — Context Builder secret-safety pass (#3):
//! raw secrets injected DIRECTLY after candidate generation/validation are
//! blocked by the builder itself, independent of upstream Phase 1B layers.

mod common;

use attic_evidence::{AuthorityLevel, Evidence, EvidenceSourceType as ST};
use attic_retrieval::context;

#[test]
fn context_builder_blocks_secret_injected_directly_after_validation() {
    const RAW_SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

    let mut secret_ev = Evidence::new("ev-secret", "repo-x");
    secret_ev.source_type = ST::Configuration;
    secret_ev.path = "config/app.yml".to_owned();
    secret_ev.source_revision_id = Some("rev-1".to_owned());
    secret_ev.index_generation_id = Some("gen-1".to_owned());
    secret_ev.authority = AuthorityLevel::Configured;
    secret_ev.confidence = 0.9;
    secret_ev.signals.combined_score = Some(0.9);
    secret_ev.snippet = Some(format!("aws_access_key_id = \"{RAW_SECRET}\""));

    let mut benign_ev = Evidence::new("ev-benign", "repo-x");
    benign_ev.source_type = ST::Configuration;
    benign_ev.path = "config/other.yml".to_owned();
    benign_ev.source_revision_id = Some("rev-1".to_owned());
    benign_ev.index_generation_id = Some("gen-1".to_owned());
    benign_ev.authority = AuthorityLevel::Configured;
    benign_ev.confidence = 0.8;
    benign_ev.signals.combined_score = Some(0.8);
    benign_ev.snippet = Some("server:\n  port: 8443".to_owned());

    let validated = vec![secret_ev, benign_ev];
    let qt = attic_retrieval::classify("what is the port setting")
        .unwrap()
        .query_type;
    let doc = context::build(&validated, &[], qt, 16_384, Some(ST::Configuration));

    // Raw secret never escapes the assembled text.
    assert!(!doc.text.contains(RAW_SECRET), "RAW SECRET ESCAPED CONTEXT");
    // The offending item produced NO served ref.
    assert!(doc.refs.iter().all(|r| r.evidence_id != "ev-secret"));
    assert!(doc.refs.iter().any(|r| r.evidence_id == "ev-benign"));
    // Truthful drop accounting with the mandated reason:
    assert!(doc.dropped.iter().any(
        |d| d.evidence_id == "ev-secret" && d.drop_reason.as_str() == "SECRET_CONTENT_DETECTED"
    ));
    // RP-INV-7 accounting: served refs are the only token contributors the
    // plan records; total doc tokens must cover them.
    let sum: u64 = doc.refs.iter().map(|r| r.token_count as u64).sum();
    assert!(
        doc.tokens >= sum,
        "doc tokens {} < ref sum {}",
        doc.tokens,
        sum
    );
}
