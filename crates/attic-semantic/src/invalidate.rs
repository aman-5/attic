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
    /// Selected units still waiting for an embedding (queued or in flight).
    pub enqueued: usize,
    /// Of those, units that had no queue row before this pass.
    pub newly_enqueued: usize,
    /// Rows made retrievable from an already-stored vector (no inference).
    pub reused: usize,
    /// Occurrences removed because their unit left the index/selection.
    pub pruned_occurrences: usize,
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
    // 2. Recompute the expected selection over the CURRENT index, reading
    //    EVERY selectable unit in keyset pages. The caps apply to what is
    //    selected (after dedup/exclusions), never to the raw scan: a raw
    //    `LIMIT max_units_total` silently left whole parts of large
    //    workspaces out of semantic search. Each page's texts are hashed and
    //    dropped, so memory stays bounded by row metadata, not by text.
    let demand = selection::demand_from_store(Some(store));
    let mut rows: Vec<attic_storage::SemanticUnitRow> = Vec::new();
    let mut after = String::new();
    let mut scan_truncated = false;
    loop {
        let page = attic_storage::semantic_unit_rows_after(conn, &after, SCAN_PAGE_ROWS)?;
        let full_page = page.len() == SCAN_PAGE_ROWS as usize;
        if let Some(last) = page.last() {
            after = last.unit_id.clone();
        }
        rows.extend(page.into_iter().map(compact_row));
        if !full_page {
            break;
        }
        if rows.len() >= SCAN_SAFETY_ROWS {
            scan_truncated = true;
            tracing::warn!(
                rows = rows.len(),
                "semantic reconcile stopped at the scan safety bound; the rest of the index is not considered this pass"
            );
            break;
        }
    }
    let (selected, duplicates, mut sel_report) = selection::select_units(&rows, &demand, sel_cfg);
    sel_report.scan_truncated = scan_truncated;
    selection::publish_selection_report(&sel_report);
    report.selection = sel_report;

    // Expected projection identity per unit under the active model. Stored
    // rows carry the CANONICAL hash (commit and orphan projection both write
    // it), so that is what must be compared — hashing the decorated
    // retrieval text made every JSON/AEM unit look stale on every pass.
    // Duplicate-content units are expected too: they are projected from the
    // winning unit's vector, and treating them as stale deleted ~all of them
    // after every drive slice only for the next slice to re-project them.
    let mut expected: std::collections::HashMap<&str, (&str, &str)> =
        std::collections::HashMap::with_capacity(selected.len() + duplicates.len());
    for su in &selected {
        expected.insert(
            su.row.unit_id.as_str(),
            (
                su.row.canonical_hash.as_deref().unwrap_or_default(),
                su.row.index_generation_id.as_str(),
            ),
        );
    }
    for (drow, hash) in &duplicates {
        expected.insert(
            drow.unit_id.as_str(),
            (hash.as_str(), drow.index_generation_id.as_str()),
        );
    }

    // 3. Partition stored rows into still-valid and stale.
    let stored = store.active_identity_rows(provider.id(), provider.model_id())?;
    let mut kept_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stale: Vec<String> = Vec::new();
    for s in stored {
        match expected.get(s.retrieval_unit_id.as_str()) {
            Some((ch, genid))
                if *ch == s.content_hash
                    && *genid == s.index_generation_id
                    && s.selection_version == selection::SEMANTIC_SELECTION_VERSION =>
            {
                kept_ids.insert(s.retrieval_unit_id);
            }
            _ => stale.push(s.retrieval_unit_id),
        }
    }

    // 4. What is selected but not yet embedded (score = priority).
    let missing: Vec<&selection::SelectedUnit> = selected
        .iter()
        .filter(|su| !kept_ids.contains(&su.row.unit_id))
        .collect();

    // 6. Bounded queue hygiene: only currently-selected units that still
    //    need inference stay claimable. Content whose vector already exists
    //    (see step 7) leaves the queue: it is projected directly instead of
    //    riding in a GPU batch.
    let vsid = fp.vector_space_id();
    let have_vector = store.canonical_hashes_with_vectors(&vsid)?;
    let reusable = |su: &&selection::SelectedUnit| {
        !kept_ids.contains(&su.row.unit_id)
            && have_vector.contains(su.row.canonical_hash.as_deref().unwrap_or_default())
    };
    let claimable: Vec<String> = selected
        .iter()
        .filter(|su| !reusable(su))
        .map(|s| s.row.unit_id.clone())
        .collect();
    report.queue_dropped = store.queue_retain_only(&claimable)?;

    // 7. Identity registration + enqueue (r02/§5). Every selected unit not
    // yet embedded gets a durable occurrence linked to this vector space +
    // its canonical hash, then a leased/fenced queue entry. Duplicate units
    // get an occurrence linked to the SAME canonical hash and no queue entry.
    // Occurrences already registered with the same hash are not rewritten.
    let cgid = fp.content_generation_id(selection::SEMANTIC_SELECTION_VERSION);
    let registered = store.occurrence_hashes(&vsid)?;
    // Content already embedded in this vector space (e.g. every unchanged
    // chunk after a re-index mints fresh unit ids) needs no inference: it is
    // registered WITHOUT a queue row and projected from the stored vector
    // below. Queueing it mixed ~35k instant items into GPU batches, each
    // still paying the batch's fixed inference cost.
    let mut reused = 0usize;
    let mut records: Vec<crate::store::NewOccurrence<'_>> =
        Vec::with_capacity(missing.len() + duplicates.len());
    for su in &missing {
        let hash = su.row.canonical_hash.as_deref().unwrap_or_default();
        let has_vector = have_vector.contains(hash);
        if has_vector {
            reused += 1;
            if registered.get(&su.row.unit_id).map(String::as_str) == Some(hash) {
                continue;
            }
        }
        records.push(crate::store::NewOccurrence {
            occurrence_id: &su.row.unit_id,
            vector_space_id: &vsid,
            canonical_hash: hash,
            repository_id: &su.row.repository_id,
            source_revision_id: &su.row.source_revision_id,
            index_generation_id: &su.row.index_generation_id,
            content_generation_id: &cgid,
            enqueue_priority: (!has_vector).then_some(su.score),
        });
    }
    for (drow, hash) in &duplicates {
        if registered.get(&drow.unit_id) == Some(hash) {
            continue;
        }
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
    report.newly_enqueued = store.add_occurrences_and_enqueue(&records)?;

    // 8. Occurrences of units that are no longer selected or linked (removed
    //    files, re-indexed ids, deselected units). Skipped when the scan was
    //    truncated: units past the bound are unknown, not gone.
    if !scan_truncated {
        let keep: std::collections::HashSet<&str> = selected
            .iter()
            .map(|s| s.row.unit_id.as_str())
            .chain(duplicates.iter().map(|(d, _)| d.unit_id.as_str()))
            .collect();
        report.pruned_occurrences = store.prune_occurrences(&vsid, &keep)?;
    }

    // 9. Make reused content retrievable now rather than after the next
    //    drive slice (after step 8, so pruned occurrences are never
    //    re-projected): project every linked occurrence whose vector already exists.
    if reused > 0 || !duplicates.is_empty() {
        let gen_id = crate::enrich::ensure_generation_for_fingerprint(store, &fp)?;
        report.reused = store.project_resolved_orphan_occurrences(
            gen_id,
            provider.id(),
            provider.model_id(),
            selection::SEMANTIC_SELECTION_VERSION,
        )? as usize;
    }

    // 10. Delete stale projection rows in one transaction — LAST, after the
    //     replacement rows exist, so semantic search never sees a window
    //     with neither (a re-index renames every unit; deleting first left
    //     search empty for the whole ~80 s pass, or until the next pass if
    //     the process stopped in between). A full re-index
    //    mints fresh unit ids for unchanged text, but that never costs
    //    inference again: the canonical vector lives in `sem_embeddings_v2`,
    //    keyed by vector space + canonical hash, and enrichment reuses it.
    report.invalidated_stale =
        store.delete_projections(&stale, provider.id(), provider.model_id())?;

    report.enqueued = missing.len() - reused;
    Ok(report)
}

/// Rows read per keyset page during reconcile.
const SCAN_PAGE_ROWS: u32 = 20_000;

/// Safety bound on units read in one reconcile pass. Far above the
/// 500,000-unit embedding cap; exists only so a pathological index cannot
/// exhaust memory. Hitting it is reported (`scan_truncated`).
pub const SCAN_SAFETY_ROWS: usize = 5_000_000;

/// Reduce a scanned row to what selection and registration use: the canonical
/// hash is computed (when the index has none), then the texts and fields
/// selection never reads are dropped.
fn compact_row(mut r: attic_storage::SemanticUnitRow) -> attic_storage::SemanticUnitRow {
    if r.canonical_hash.is_none() {
        r.canonical_hash = Some(crate::identity::content_hash(&r.canonical_text));
    }
    r.canonical_text = String::new();
    r.retrieval_text = String::new();
    r.lexical_state = String::new();
    r.freshness_state = String::new();
    r.content_hash = String::new();
    r.file_occurrence_id = String::new();
    r
}
