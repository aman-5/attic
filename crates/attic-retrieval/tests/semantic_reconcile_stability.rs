//! Reconcile must be a fixed point once everything is embedded.
//!
//! Duplicate-content units and decorated units (JSON pointer headers) used to
//! be classified stale on every pass: reconcile deleted their projection rows,
//! the next drive slice re-projected them, and the cycle repeated for the whole
//! run, leaving the GPU idle half the time. These tests pin the fixed point.

mod common;

use std::sync::Arc;

use attic_retrieval::semantic::SemanticStack;
use attic_semantic::{CancelFlag, EnrichmentConfig, SemanticProvider, testing::HashingEmbedder};
use common::Fixture;

const SHARED: &str =
    "pub fn shared_helper(x: u32) -> u32 {\n    x.wrapping_mul(31).rotate_left(7)\n}\n";
const ENV_JSON: &str =
    r#"{"journey":{"id":"EKYC","steps":["otp","pan","aadhaar"]},"retry":{"max":3}}"#;

fn selection() -> attic_semantic::SelectionConfig {
    attic_semantic::SelectionConfig {
        min_score: 0.0,
        ..Default::default()
    }
}

#[test]
fn second_reconcile_after_full_embedding_changes_nothing() {
    // Identical files (duplicates) and identical JSON exports under
    // different names (decorated units whose retrieval text differs from the
    // canonical body).
    let fx = Fixture::seed_pub(&[
        ("src/a.rs", SHARED),
        ("src/b.rs", SHARED),
        ("src/c.rs", SHARED),
        ("config/DEV-Form.json", ENV_JSON),
        ("config/PROD-Form.json", ENV_JSON),
        (
            "docs/readme.md",
            "# Retry\nThe EKYC journey retries OTP up to three times.\n",
        ),
    ]);
    let stack = SemanticStack::open(
        &fx.dir.path().join("semantic.db"),
        Arc::new(HashingEmbedder::new()) as Arc<dyn SemanticProvider>,
    )
    .expect("semantic stack");
    let conn = fx.read_conn();

    let first =
        attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
            .unwrap();
    assert!(first.enqueued > 0, "{first:?}");
    assert!(
        first
            .selection
            .excluded
            .get("duplicate_content")
            .copied()
            .unwrap_or(0)
            > 0,
        "fixture must contain duplicates: {first:?}"
    );

    // Drain the queue (each drive ends with the orphan projection pass that
    // gives every duplicate its own retrievable row).
    for _ in 0..20 {
        attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig::standalone(16, 3, 60_000, 1),
            &CancelFlag::new(),
        )
        .unwrap();
        if stack.store.queue_counts().unwrap().pending == 0 {
            break;
        }
    }
    assert_eq!(stack.store.queue_counts().unwrap().pending, 0);

    for pass in 0..2 {
        let again =
            attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
                .unwrap();
        assert_eq!(
            again.invalidated_stale, 0,
            "pass {pass}: nothing may be stale: {again:?}"
        );
        assert_eq!(
            again.newly_enqueued, 0,
            "pass {pass}: nothing to queue: {again:?}"
        );
        assert_eq!(again.pruned_occurrences, 0, "pass {pass}: {again:?}");
        assert_eq!(
            again.enqueued, 0,
            "pass {pass}: everything embedded: {again:?}"
        );
        // A drive after a no-op reconcile must not re-project anything.
        attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig::standalone(16, 3, 60_000, 1),
            &CancelFlag::new(),
        )
        .unwrap();
    }

    // Every selected AND duplicate unit stays retrievable.
    let projected = stack
        .store
        .count(stack.provider.id(), stack.provider.model_id(), None)
        .unwrap() as i64;
    let report =
        attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
            .unwrap();
    let dups = report
        .selection
        .excluded
        .get("duplicate_content")
        .copied()
        .unwrap_or(0) as i64;
    assert_eq!(projected, report.selection.selected as i64 + dups);
}

#[test]
fn reindexed_unchanged_content_is_reused_without_queueing_inference() {
    let fx = Fixture::seed_pub(&[
        ("src/a.rs", SHARED),
        (
            "docs/readme.md",
            "# Retry\nThe EKYC journey retries OTP up to three times.\n",
        ),
    ]);
    let stack = SemanticStack::open(
        &fx.dir.path().join("semantic.db"),
        Arc::new(HashingEmbedder::new()) as Arc<dyn SemanticProvider>,
    )
    .expect("semantic stack");
    let conn = fx.read_conn();
    attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection()).unwrap();
    attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig::standalone(16, 3, 60_000, 1),
        &CancelFlag::new(),
    )
    .unwrap();
    let embedded_before = stack
        .store
        .count(stack.provider.id(), stack.provider.model_id(), None)
        .unwrap();

    // A full re-index mints fresh unit ids for byte-identical content.
    fx.writer
        .send(|c| {
            c.execute(
                "UPDATE core_retrieval_units SET id = id || '-reindexed'",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    let r = attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
        .unwrap();
    assert_eq!(
        r.newly_enqueued, 0,
        "no inference for unchanged content: {r:?}"
    );
    assert_eq!(r.enqueued, 0, "{r:?}");
    assert!(r.reused > 0, "{r:?}");
    assert_eq!(stack.store.queue_counts().unwrap().pending, 0);
    assert_eq!(
        stack
            .store
            .count(stack.provider.id(), stack.provider.model_id(), None)
            .unwrap(),
        embedded_before,
        "every re-indexed unit is retrievable immediately"
    );
}

#[test]
fn units_removed_from_the_index_lose_their_occurrences_once() {
    let fx = Fixture::seed_pub(&[("src/a.rs", SHARED), ("src/b.rs", SHARED)]);
    let stack = SemanticStack::open(
        &fx.dir.path().join("semantic.db"),
        Arc::new(HashingEmbedder::new()) as Arc<dyn SemanticProvider>,
    )
    .expect("semantic stack");
    let conn = fx.read_conn();
    attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection()).unwrap();
    attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig::standalone(16, 3, 60_000, 1),
        &CancelFlag::new(),
    )
    .unwrap();

    // The duplicate's unit leaves the canonical index.
    fx.writer
        .send(|c| {
            c.execute(
                "UPDATE core_retrieval_units SET lexical_state = 'INVALID'
                  WHERE file_occurrence_id IN
                    (SELECT id FROM core_file_occurrences WHERE path = 'src/b.rs')",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    let gone =
        attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
            .unwrap();
    assert!(gone.pruned_occurrences >= 1, "{gone:?}");
    // Not resurrected by the next slice, not deleted again by the next pass.
    attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig::standalone(16, 3, 60_000, 1),
        &CancelFlag::new(),
    )
    .unwrap();
    let after =
        attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &selection())
            .unwrap();
    assert_eq!(after.invalidated_stale, 0, "{after:?}");
    assert_eq!(after.pruned_occurrences, 0, "{after:?}");
}
