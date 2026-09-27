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
    /// Stale rows kept as reuse donors: their content hash is selected again
    /// under a fresh unit id (a re-index mints new ids for unchanged text),
    /// so enrichment clones the stored vector instead of re-running
    /// inference. A later reconcile sweeps them once the replacement exists.
    pub reuse_donors_kept: usize,
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
    let (selected, duplicates, sel_report) = selection::select_units(&rows, &demand, sel_cfg);
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

    // 5. Delete stale rows — EXCEPT reuse donors. A full re-index mints fresh
    //    unit ids for unchanged text, which would otherwise read as
    //    "everything stale" and force a full CPU re-embed of content that has
    //    not changed by one byte. A stale row whose content hash is selected
    //    again stays: enrichment clones its stored vector onto the new unit id
    //    via `vectors_for_contents`. The donor is swept by a later reconcile
    //    once the replacement row exists (its hash is then no longer missing).
    let missing_hashes: std::collections::HashSet<&str> = missing
        .iter()
        .map(|(id, _)| expected[id.as_str()].0.as_str())
        .collect();
    for s in stale {
        if missing_hashes.contains(s.content_hash.as_str()) {
            report.reuse_donors_kept += 1;
            continue;
        }
        store.delete(
            &s.retrieval_unit_id,
            Some(provider.id()),
            Some(provider.model_id()),
        )?;
        report.invalidated_stale += 1;
    }

    // 6. Bounded queue hygiene: drop entries no longer selected.
    let all_selected: Vec<String> = selected.iter().map(|s| s.row.unit_id.clone()).collect();
    report.queue_dropped = store.queue_retain_only(&all_selected)?;
    // v2 queue gets the same "only currently-selected units stay claimable"
    // hygiene as v1 above — otherwise a deselected unit's PENDING v2 row
    // (occurrence_id == retrieval_unit_id) would stay claimable forever.
    store.queue_v2_retain_only(&all_selected)?;

    // 7. v2 identity registration + enqueue (r02/§5, Phase 1). Every selected
    // unit not yet embedded gets a durable occurrence (repo/path/revision/
    // generation provenance) linked to this vector space + its canonical
    // hash, then an entry in the leased/fenced v2 queue. Units that lost the
    // canonical-dedup tiebreak (`duplicates`) still get an occurrence linked
    // to the SAME canonical hash — no queue entry, since the winning unit's
    // completion already produces that vector — so their repo/path/env
    // provenance is never silently dropped by deduplication.
    //
    // A provider with no fingerprint (test doubles such as `OomProvider`,
    // `HashingEmbedder` — no real production provider lacks one; see
    // Phase 2's identity gate) has no stable vector-space identity to key
    // occurrences by, so it falls back to the v1 queue entirely.
    if let Some(fp) = provider.fingerprint() {
        let vsid = store.ensure_vector_space(&fp, &fp.quantization)?;
        let cgid = store.ensure_content_generation(&fp, selection::SEMANTIC_SELECTION_VERSION)?;

        let missing_ids: std::collections::HashSet<&str> =
            missing.iter().map(|(id, _)| id.as_str()).collect();
        for su in &selected {
            if !missing_ids.contains(su.row.unit_id.as_str()) {
                continue;
            }
            let hash = su
                .row
                .canonical_hash
                .clone()
                .unwrap_or_else(|| crate::identity::content_hash(&su.row.canonical_text));
            store.add_occurrence(
                &su.row.unit_id,
                &su.row.unit_id,
                &vsid,
                &hash,
                &su.row.repository_id,
                &su.row.source_revision_id,
                &su.row.index_generation_id,
                &cgid,
                "{}",
            )?;
            store.queue_v2_enqueue(&su.row.unit_id, su.score)?;
        }
        for (drow, hash) in &duplicates {
            store.add_occurrence(
                &drow.unit_id,
                &drow.unit_id,
                &vsid,
                hash,
                &drow.repository_id,
                &drow.source_revision_id,
                &drow.index_generation_id,
                &cgid,
                "{}",
            )?;
        }
    } else if !missing.is_empty() {
        store.queue_enqueue_scored(&missing)?;
    }

    if !missing.is_empty() {
        report.enqueued = missing.len();
    }

    Ok(report)
}
