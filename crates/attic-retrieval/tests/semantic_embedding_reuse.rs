//! Regression: re-indexing unchanged content must not pay CPU inference again.
//!
//! Unit ids are minted fresh on every indexing generation, so a full re-index
//! makes every stored embedding identity-stale even when not one byte of
//! content changed — historically this re-embedded the entire workspace
//! (measured on a real corpus: the full queue re-enqueued after a restart).
//! Canonical vectors are keyed by vector space + content hash, independent of
//! unit ids, so enrichment reuses them for the new units: the provider must
//! see zero texts on the second pass.

mod common;

use std::sync::Arc;

use attic_semantic::{
    CancelFlag, EnrichmentConfig, SelectionConfig, SemanticStore, testing::RecordingProvider,
};
use common::Fixture;

const SHARED_PY: &str = r#"import os
import sys


def process_payment(amount_cents: int, currency: str) -> bool:
    """Charge via the payment provider; returns success."""
    if amount_cents <= 0:
        return False
    return True


def refund_payment(order_id: str) -> bool:
    """Refund a previously captured charge."""
    return bool(order_id)
"#;

const SHARED_MD: &str = r#"# Payments runbook

## Charge flow

The charge endpoint validates the amount, calls the provider, and records
the outcome. Refunds follow the same path in reverse with an audit entry.

## Retries

Transient provider errors are retried with backoff; permanent ones alert.
"#;

fn full() -> EnrichmentConfig {
    EnrichmentConfig::standalone(16, 3, 10_000, 1)
}

#[test]
fn reindexed_unchanged_content_is_reused_not_re_embedded() {
    let fx = Fixture::seed_pub(&[
        ("services/pay.py", SHARED_PY),
        ("docs/runbook.md", SHARED_MD),
    ]);
    let provider = Arc::new(RecordingProvider {
        vectors: vec![vec![0.25, 0.5, 0.75, 1.0]],
        seen_texts: std::sync::Mutex::new(Vec::new()),
    });
    let store = SemanticStore::open(&fx.dir.path().join("semantic.db")).expect("semantic store");

    // First pass: the provider embeds every selected unit.
    let first_embedded = {
        let conn = fx.read_conn();
        attic_semantic::reconcile(
            &conn,
            &store,
            provider.as_ref(),
            &SelectionConfig::default(),
        )
        .unwrap();
        let stats = attic_semantic::drive(
            &conn,
            &store,
            provider.as_ref(),
            &full(),
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(stats.embedded > 0, "first pass must embed the selection");
        stats.embedded
    };
    let seen_after_first = provider.seen_texts.lock().unwrap().len();
    assert!(seen_after_first > 0);

    // Re-index byte-identical content: new generation, fresh unit ids. The
    // options mirror the fixture's own bootstrap (`repository_name: phase4`).
    attic_indexing::index_repository(
        &fx.store(),
        &fx.root,
        &attic_discovery::DiscoveryPolicy::default_git(),
        &attic_indexing::IndexOptions {
            repository_name: "phase4".into(),
            ..Default::default()
        },
    )
    .expect("re-index");

    // Second pass: same content under new identities. Reconcile enqueues the
    // new units; drive must satisfy every one of them from the canonical
    // vectors — the provider sees nothing new.
    {
        let conn = fx.read_conn();
        let report = attic_semantic::reconcile(
            &conn,
            &store,
            provider.as_ref(),
            &SelectionConfig::default(),
        )
        .unwrap();
        assert!(
            report.enqueued > 0,
            "the new generation's units must be enqueued: {report:?}"
        );
        let stats = attic_semantic::drive(
            &conn,
            &store,
            provider.as_ref(),
            &full(),
            &CancelFlag::new(),
        )
        .unwrap();
        assert!(
            stats.embedded > 0,
            "the new generation's units must still be written, via reuse"
        );
    }

    let seen_after_second = provider.seen_texts.lock().unwrap().len();
    assert_eq!(
        seen_after_second, seen_after_first,
        "re-indexing unchanged content must cost zero additional inference"
    );

    // And the new generation's units really do have stored vectors now.
    let conn = fx.read_conn();
    let rows = attic_storage::semantic_unit_rows(&conn, 1_000).unwrap();
    let with_vectors = rows
        .iter()
        .filter(|r| {
            store
                .lookup(&r.unit_id, "recording", "recording-v1")
                .unwrap()
                .is_some()
        })
        .count();
    assert!(
        with_vectors as u64 >= first_embedded.min(rows.len() as u64).max(1),
        "at least the selected units of the new generation must carry vectors (found {with_vectors})"
    );
}
