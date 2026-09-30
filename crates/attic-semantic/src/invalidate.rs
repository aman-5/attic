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
///
/// A provider without a fingerprint (the unavailable placeholder used while
/// the model is still downloading, or when semantic search is disabled) has
/// no vector space, so reconcile does nothing at all — in particular it
/// never purges the real model's embeddings on its behalf.
pub fn reconcile(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    sel_cfg: &SelectionConfig,
) -> Result<ReconcileReport, crate::error::SemanticError> {
    let Some(fp) = provider.fingerprint() else {
        return Ok(ReconcileReport::default());
    };
    let mut report = ReconcileReport {
        purged_other_models: store.purge_inactive_models(provider.id(), provider.model_id())?,
        ..Default::default()
    };
    // 2. Recompute the expected selection over the CURRENT index.
    let demand = selection::demand_from_store(Some(store));
    let max_units = sel_cfg.max_units_total.min(200_000) as u32;
    let rows = attic_storage::semantic_unit_rows(conn, max_units)?;
    let (selected, duplicates, sel_report) = selection::select_units(&rows, &demand, sel_cfg);
    selection::publish_selection_report(&sel_report);
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

    // 3. Partition stored rows into still-valid and stale (deletion happens
    //    after the missing set is known — see step 5).
    let stored = store.active_identity_rows(provider.id(), provider.model_id())?;
    let mut kept_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stale = Vec::new();
    for s in stored {
        match expected.get(s.retrieval_unit_id.as_str()) {
            Some((ch, genid, ver))
                if *ch == s.content_hash
                    && *genid == s.index_generation_id
                    && *ver == s.selection_version =>
            {
                kept_ids.insert(s.retrieval_unit_id);
            }
            _ => stale.push(s),
        }
    }

    // 4. Enqueue what is selected but not yet embedded (score = priority).
    let mut missing: Vec<(String, f64)> = Vec::new();
    for su in &selected {
        if !kept_ids.contains(&su.row.unit_id) {
            missing.push((su.row.unit_id.clone(), su.score));
        }
    }

    // 5. Delete stale projection rows. A full re-index mints fresh unit ids
    //    for unchanged text, but that never costs inference again: the
    //    canonical vector lives in `sem_embeddings_v2`, keyed by vector space
    //    + canonical hash, independent of these per-unit projection rows, and
    //    enrichment reuses it for every new unit with the same content.
    for s in stale {
        store.delete(
            &s.retrieval_unit_id,
            Some(provider.id()),
            Some(provider.model_id()),
        )?;
        report.invalidated_stale += 1;
    }

    // 6. Bounded queue hygiene: only currently-selected units stay claimable.
    let all_selected: Vec<String> = selected.iter().map(|s| s.row.unit_id.clone()).collect();
    report.queue_dropped = store.queue_retain_only(&all_selected)?;

    // 7. Identity registration + enqueue (r02/§5). Every selected unit not
    // yet embedded gets a durable occurrence (repo/path/revision/generation
    // provenance) linked to this vector space + its canonical hash, then an
    // entry in the leased/fenced queue. Units that lost the canonical-dedup
    // tiebreak (`duplicates`) still get an occurrence linked to the SAME
    // canonical hash — no queue entry, since the winning unit's completion
    // already produces that vector — so their repo/path/env provenance is
    // never silently dropped by deduplication.
    let vsid = fp.vector_space_id();
    let cgid = fp.content_generation_id(selection::SEMANTIC_SELECTION_VERSION);

    let missing_ids: std::collections::HashSet<&str> =
        missing.iter().map(|(id, _)| id.as_str()).collect();
    // One transaction for every occurrence + queue row. Row-at-a-time
    // autocommit writes took ~30 s for a 45K-unit repository before the
    // first chunk could be embedded.
    let mut records: Vec<crate::store::NewOccurrence<'_>> =
        Vec::with_capacity(missing.len() + duplicates.len());
    let hashes: Vec<String> = selected
        .iter()
        .map(|su| {
            su.row
                .canonical_hash
                .clone()
                .unwrap_or_else(|| crate::identity::content_hash(&su.row.canonical_text))
        })
        .collect();
    for (su, hash) in selected.iter().zip(&hashes) {
        if !missing_ids.contains(su.row.unit_id.as_str()) {
            continue;
        }
        records.push(crate::store::NewOccurrence {
            occurrence_id: &su.row.unit_id,
            vector_space_id: &vsid,
            canonical_hash: hash,
            repository_id: &su.row.repository_id,
            source_revision_id: &su.row.source_revision_id,
            index_generation_id: &su.row.index_generation_id,
            content_generation_id: &cgid,
            enqueue_priority: Some(su.score),
        });
    }
    for (drow, hash) in &duplicates {
        records.push(crate::store::NewOccurrence {
            occurrence_id: &drow.unit_id,
            vector_space_id: &vsid,
            canonical_hash: hash,
            repository_id: &drow.repository_id,
            source_revision_id: &drow.source_revision_id,
            index_generation_id: &drow.index_generation_id,
            content_generation_id: &cgid,
            enqueue_priority: None,
        });
    }
    store.add_occurrences_and_enqueue(&records)?;

    report.enqueued = missing.len();
    Ok(report)
}
