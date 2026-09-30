//! Phase 4 hardening proofs — filesystem byte/file budgets (#2): source
//! verification charges ACTUAL sanitized bytes, stops at the remaining
//! budget, and reports observable degradation. Includes LARGE streaming.

mod common;

use attic_evidence::{AuthorityLevel, Evidence, VerificationStatus};
use attic_retrieval::budget::BudgetAccountant;
use attic_retrieval::mode::{AnswerMode as AM, AnswerModePolicy, VerificationLevel};
use attic_retrieval::verify::{self, VerifyOutcome};
use common::Fixture;

fn policy_with_fs(files: u32, bytes: u64) -> AnswerModePolicy {
    let mut p = AnswerModePolicy::for_mode(AM::Normal);
    p.max_fs_files = files;
    p.max_fs_bytes = bytes;
    p.source_verification_level = VerificationLevel::Checksum;
    p.validate().expect("policy valid");
    p
}

fn evidence_for(path: &str, start: u32, end: u32, snippet: &str) -> Evidence {
    let mut ev = Evidence::new("ev-under-test", "00000000-0000-0000-0000-000000000000");
    ev.path = path.to_owned();
    ev.source_span = Some(attic_core::SourceSpan::new(start, 0, end, 0));
    ev.snippet = Some(snippet.to_owned());
    ev.authority = AuthorityLevel::Implementation;
    ev
}

#[test]
fn small_file_overrun_stops_at_byte_budget_and_reports_degradation() {
    let fx = Fixture::bootstrap();
    // 8 KiB file whose marker sits at the very END.
    let body = format!(
        "{}\nMARKER_TOKEN_AT_END_XYZ\n",
        "filler line for budget testing\n".repeat(300)
    );
    std::fs::write(fx.root.join("config/app.yml"), &body).unwrap();

    let mut ev = evidence_for("config/app.yml", 1, 400, "MARKER_TOKEN_AT_END_XYZ");
    let policy = policy_with_fs(5, 2048); // far below 8 KiB
    let mut budget = BudgetAccountant::new(&policy);

    let outcome = verify::verify_evidence(
        &mut ev,
        &fx.root.canonicalize().unwrap(),
        &policy,
        &mut budget,
    )
    .expect("verify");

    assert_eq!(outcome, VerifyOutcome::BlockedByBudget);
    assert!(
        budget.fs_bytes_used <= policy.max_fs_bytes,
        "byte budget exceeded: {} > {}",
        budget.fs_bytes_used,
        policy.max_fs_bytes
    );
    assert!(
        budget.fs_bytes_used > 0,
        "actual consumption must be charged"
    );
    assert_eq!(budget.fs_files_used, 1);
    assert!(budget.limits_hit().iter().any(|l| l.contains("fs_bytes")));
    assert!(!ev.live_source_verified, "no verdict from truncated read");
    assert_ne!(ev.verification_state, VerificationStatus::Verified);
}

#[test]
fn large_streamed_verification_stops_within_byte_budget_when_marker_unreachable() {
    let fx = Fixture::bootstrap();
    // >4 MiB LARGE file; marker only near the END.
    let prefix = "x".repeat(4 * 1024 * 1024 - 4096);
    let body = format!(
        "{prefix}\nLATE_MARKER_AFTER_BUDGET_QQ\n{}\n",
        "y\n".repeat(100)
    );
    std::fs::create_dir_all(fx.root.join("services")).unwrap();
    std::fs::write(fx.root.join("services/big_generated.log"), &body).unwrap();

    let mut ev = evidence_for(
        "services/big_generated.log",
        1,
        200_000,
        "LATE_MARKER_AFTER_BUDGET_QQ",
    );
    let policy = policy_with_fs(3, 64 * 1024); // << 4 MiB prefix
    let mut budget = BudgetAccountant::new(&policy);

    let outcome = verify::verify_evidence(
        &mut ev,
        &fx.root.canonicalize().unwrap(),
        &policy,
        &mut budget,
    )
    .expect("verify");

    assert_eq!(outcome, VerifyOutcome::BlockedByBudget);
    assert!(
        budget.fs_bytes_used <= policy.max_fs_bytes,
        "streaming exceeded byte budget: {} > {}",
        budget.fs_bytes_used,
        policy.max_fs_bytes
    );
    assert!(!ev.live_source_verified);
}

#[test]
fn large_streamed_verification_succeeds_when_marker_within_budget() {
    let fx = Fixture::bootstrap();
    // Marker in the FIRST kilobyte of a LARGE file: early containment must
    // succeed inside a modest budget.
    let marker_line = "EARLY_MARKER_WITHIN_BUDGET_AA";
    let body = format!("{marker_line}\n{}\n", "y\n".repeat(4 * 1024 * 1024));
    std::fs::write(fx.root.join("services/big2.log"), &body).unwrap();

    let mut ev = evidence_for("services/big2.log", 1, 50, marker_line);
    let policy = policy_with_fs(3, 256 * 1024);
    let mut budget = BudgetAccountant::new(&policy);

    let outcome = verify::verify_evidence(
        &mut ev,
        &fx.root.canonicalize().unwrap(),
        &policy,
        &mut budget,
    )
    .expect("verify");

    assert_eq!(outcome, VerifyOutcome::VerifiedCurrent);
    assert!(ev.live_source_verified);
    assert!(budget.fs_bytes_used <= policy.max_fs_bytes);
}

#[test]
fn file_slot_budget_blocks_second_verification() {
    let fx = Fixture::bootstrap();
    let root = fx.root.canonicalize().unwrap();

    let mut a = evidence_for("config/app.yml", 1, 10, "port");
    let mut b = evidence_for("docs/runbook.md", 1, 10, "database_url");
    let policy = policy_with_fs(1, 1024 * 1024); // one file slot only

    let mut budget = BudgetAccountant::new(&policy);
    let r1 = verify::verify_evidence(&mut a, &root, &policy, &mut budget).expect("first");
    assert!(matches!(
        r1,
        VerifyOutcome::VerifiedCurrent | VerifyOutcome::ContentChanged
    ));
    assert_eq!(budget.fs_files_used, 1);

    let r2 = verify::verify_evidence(&mut b, &root, &policy, &mut budget).expect("second");
    assert_eq!(r2, VerifyOutcome::BlockedByBudget, "file cap must block");
    assert_eq!(
        budget.fs_files_used, 1,
        "file count must never exceed max_fs_files"
    );
}
