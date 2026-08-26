//! Incremental semantic invalidation (Phase 5 §10) + full rebuild.
//!
//! Reconcile is the SINGLE entry point that keeps the disposable layer
//! aligned with the canonical index:
//!
//! ```text
//! source edit            → content_hash mismatch  → affected rows rebuilt
//! segmentation change    → generation/selection mismatch → rebuilt
//! embedding model change → purge_inactive_models  → other models removed
//! ranking change         → NOTHING (no embedding rebuild)
//! ```
//!
//! Unaffected workspace content is never re-embedded: units whose stored
//! identity still matches the expectation keep their vectors.

use rusqlite::Connection;

use crate::provider::SemanticProvider;
use crate::selection::{self, SelectionConfig, SelectionReport};
use crate::store::SemanticStore;

#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    /// Rows deleted because their identity no longer matches the index
    /// (stale source, changed segmentation, changed selection version).
    pub invalidated_stale: usize,
    /// Rows of INACTIVE provider/model pairs removed.
    pub purged_other_models: usize,
    /// Units (re-)queued for enrichment.
    pub enqueued: usize,
    /// Queue entries dropped for units no longer selected.
    pub queue_dropped: usize,
    /// The underlying selection report (inspectability §4/§21).
    pub selection: SelectionReport,
}

/// Bring the semantic store in line with the canonical index for the ACTIVE
/// provider/model. Idempotent; safe to run after every indexing generation.
pub fn reconcile(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    sel_cfg: &SelectionConfig,
) -> Result<ReconcileReport, crate::error::SemanticError> {
    let mut report = ReconcileReport {
        purged_other_models: store.purge_inactive_models(provider.id(), provider.model_id())?,
        ..Default::default()
    };
    // 2. Recompute the expected selection over the CURRENT index.
    let demand = selection::demand_from_store(Some(store));
    let max_units = sel_cfg.max_units_total.min(200_000) as u32;
    let rows = attic_storage::semantic_unit_rows(conn, max_units)?;
    let (selected, sel_report) = selection::select_units(&rows, &demand, sel_cfg);
    report.selection = sel_report;

    // Expected per-unit state under the active model.
    let mut expected: std::collections::HashMap<&str, (String, &str, &'static str)> =
        std::collections::HashMap::with_capacity(selected.len());
    for su in &selected {
        let ch = crate::identity::content_hash(&su.row.retrieval_text);
        expected.insert(
            su.row.unit_id.as_str(),
            (
                ch,
                su.row.index_generation_id.as_str(),
                selection::SEMANTIC_SELECTION_VERSION,
            ),
        );
    }

    // 3. Delete stored rows whose lineage no longer matches.
    let stored = store.active_identity_rows(provider.id(), provider.model_id())?;
    let mut kept_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut missing: Vec<(String, f64)> = Vec::new();
    for s in stored {
        match expected.get(s.retrieval_unit_id.as_str()) {
            Some((ch, genid, ver))
                if *ch == s.content_hash
                    && *genid == s.index_generation_id
                    && *ver == s.selection_version =>
            {
                kept_ids.insert(s.retrieval_unit_id);
            }
            _ => {
                store.delete(
                    &s.retrieval_unit_id,
                    Some(provider.id()),
                    Some(provider.model_id()),
                )?;
                report.invalidated_stale += 1;
            }
        }
    }

    // 4. Enqueue what is selected but not yet embedded (score = priority).
    for su in &selected {
        if !kept_ids.contains(&su.row.unit_id) {
            missing.push((su.row.unit_id.clone(), su.score));
        }
    }

    // 5. Bounded queue hygiene: drop entries no longer selected.
    let all_selected: Vec<String> = selected.iter().map(|s| s.row.unit_id.clone()).collect();
    report.queue_dropped = store.queue_retain_only(&all_selected)?;

    if !missing.is_empty() {
        store.queue_enqueue_scored(&missing)?;
        report.enqueued = missing.len();
    }

    Ok(report)
}
