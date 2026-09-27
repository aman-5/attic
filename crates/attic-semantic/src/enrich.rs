//! Bounded background enrichment (Phase 5 §9/§11/§20).
//!
//! Canonical indexing completes FIRST; enrichment runs afterwards as a
//! disposable, resumable, bounded job:
//! * bounded batch size, bounded drive budget, cooperative cancellation;
//! * committed embeddings are retained across restarts; INFLIGHT work is
//!   rescheduled by the store's open-time recovery; FAILED after
//!   max_attempts is quarantined;
//! * foreground queries NEVER wait on this loop (they only read the store).
//!
//! The adaptive Phase 7 scheduler is explicitly out of scope.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use attic_discovery::secrets;
use rusqlite::Connection;

use crate::error::SemanticError;
use crate::invalidate::reconcile;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, ResourceUsage, SemanticProvider,
};
use crate::selection::{SEMANTIC_SELECTION_VERSION, SelectionConfig};
use crate::store::SemanticStore;

/// Ensure an active or building generation is ready to receive vectors for this fingerprint.
/// Returns the generation ID to tag the batch with.
fn ensure_generation_for_fingerprint(
    store: &SemanticStore,
    fp: &EmbeddingFingerprint,
) -> Result<i64, SemanticError> {
    if let Some(active) = store.get_active_generation()?
        && active.fingerprint == *fp
    {
        return Ok(active.generation_id);
    }
    if let Some(building) = store.get_building_generation()?
        && building.fingerprint == *fp
    {
        return Ok(building.generation_id);
    }
    let new_gen = store.start_new_generation(fp)?;
    if store.get_active_generation()?.is_none() {
        store.activate_generation(new_gen.generation_id)?;
    }
    Ok(new_gen.generation_id)
}

/// Inspectable enrichment knobs.
#[derive(Debug, Clone)]
pub struct EnrichmentConfig {
    /// Items per embed_batch call.
    pub batch_size: usize,
    /// Attempts before an item is quarantined as FAILED.
    pub max_attempts: u32,
    /// Wall-clock budget for ONE drive() call (ms).
    pub budget_ms: u64,
    /// Number of concurrent background embedding worker threads
    /// `BackgroundEnricher::spawn` spins up (mirrors
    /// `attic_storage::ResourcePolicy::embedding_worker_count`).
    pub embedding_worker_count: usize,
    /// Maximum CPU threads available to semantic inference across all lanes.
    pub cpu_threads: usize,
    /// Optional dynamic resource allocation handle from ResourceOrchestrator (Master Plan §12, §15, CP15).
    pub dynamic_allocation: Option<Arc<std::sync::RwLock<attic_storage::ResourceAllocation>>>,
    /// Admission policy used by the reconcile pass this enricher runs: which
    /// units ever enter the queue (per-file size ceiling, path globs, caps).
    /// Carried here so the server's `[semantic]` config reaches the one place
    /// reconcile is driven from.
    pub selection: SelectionConfig,
}

impl EnrichmentConfig {
    /// Construct a standalone config with no dynamic orchestrator allocation.
    pub const fn standalone(
        batch_size: usize,
        max_attempts: u32,
        budget_ms: u64,
        embedding_worker_count: usize,
    ) -> Self {
        Self {
            batch_size,
            max_attempts,
            budget_ms,
            embedding_worker_count,
            cpu_threads: 2,
            dynamic_allocation: None,
            selection: SelectionConfig::baseline(),
        }
    }

    /// Effective batch size after checking dynamic orchestrator allocation.
    pub fn effective_batch_size(&self) -> usize {
        if let Some(ref alloc) = self.dynamic_allocation {
            let guard = alloc.read().unwrap_or_else(|e| e.into_inner());
            if guard.semantic_batch_size == 0 {
                return 0;
            }
            return guard.semantic_batch_size.min(self.batch_size);
        }
        self.batch_size
    }

    /// Effective prefetch limit after checking dynamic orchestrator allocation.
    pub fn effective_prefetch_limit(&self) -> usize {
        if let Some(ref alloc) = self.dynamic_allocation {
            let guard = alloc.read().unwrap_or_else(|e| e.into_inner());
            return guard.semantic_prefetch_limit;
        }
        self.batch_size * 2
    }
}

impl EnrichmentConfig {
    /// Effective CPU threads granted by orchestrator.
    pub fn effective_cpu_threads(&self) -> usize {
        if let Some(ref alloc) = self.dynamic_allocation {
            let guard = alloc.read().unwrap_or_else(|e| e.into_inner());
            return guard.semantic_cpu_threads;
        }
        self.cpu_threads.max(1)
    }
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            batch_size: 16,
            max_attempts: 3,
            budget_ms: 2_000,
            embedding_worker_count: 1,
            cpu_threads: 2,
            dynamic_allocation: None,
            selection: SelectionConfig::default(),
        }
    }
}

/// Observable outcome of one drive cycle (§21).
#[derive(Debug, Default, Clone)]
pub struct EnrichStats {
    pub embedded: u64,
    pub failed_items: u64,
    pub skipped_secret: u64,
    pub cancelled: bool,
    pub elapsed_ms: u64,
    pub queue_remaining: u64,
    /// How many times an OOM (BudgetExhausted) forced a batch-cap halving
    /// this drive — the adaptive-batching signal (r07).
    pub oom_reductions: u64,
}

/// Drive the enrichment queue until empty or budget/cancellation bounds hit.
///
/// `conn` is a CANONICAL READ-ONLY connection; nothing here writes to the
/// canonical database.
///
/// Production work assignment is the v2 leased/fenced queue
/// (`sem_queue_v2`): every claim carries an owner + fencing token, a killed
/// worker's stale token can never commit, and an expired lease is reclaimed
/// rather than lost. Canonical vectors are the v2 dedup unit
/// (`sem_embeddings_v2`, keyed by vector-space + canonical hash) — computed
/// at most once ever, regardless of how many occurrences share that body.
/// Each completed vector is additionally projected into the existing
/// per-generation `sem_embeddings` table so the already-proven HNSW
/// candidate index and retrieval path keep working unchanged (see
/// `SemanticStore::commit_v2_batch`).
pub fn drive(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    cfg: &EnrichmentConfig,
    cancel: &CancelFlag,
) -> Result<EnrichStats, SemanticError> {
    // A provider with no fingerprint has no stable vector-space identity to
    // key v2 occurrences/queue rows by (`reconcile` never enqueues anything
    // into v2 for it either) — every real production provider always
    // returns one; only test doubles (`OomProvider`, `HashingEmbedder`) hit
    // this fallback.
    if provider.fingerprint().is_none() {
        return drive_v1(conn, store, provider, cfg, cancel);
    }
    drive_v2(conn, store, provider, cfg, cancel)
}

fn drive_v2(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    cfg: &EnrichmentConfig,
    cancel: &CancelFlag,
) -> Result<EnrichStats, SemanticError> {
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(cfg.budget_ms.max(1));
    let mut stats = EnrichStats::default();
    // r07 OOM-adaptive cap: after a provider BudgetExhausted (GPU/native
    // OOM), batches are retried at half size (floor 1) instead of failing
    // items. Clears only when the process restarts — conservative by design.
    let mut oom_batch_cap: Option<usize> = None;

    // Crash/restart hygiene: a lease abandoned by a killed worker or a
    // server that died mid-batch surfaces here as retryable PENDING before
    // this drive claims anything new.
    store.queue_v2_reclaim_expired()?;

    // Canonical-dedup catch-up: an occurrence that lost the selection-time
    // dedup tiebreak is never queued (see `invalidate::reconcile`) — it only
    // becomes retrievable once its canonical vector exists, whether that
    // happened before this occurrence was even registered or gets produced
    // by a claim later in this very call. Run it both before and after the
    // claim loop so neither ordering leaves it stranded.
    let fp_for_orphans = provider.fingerprint();
    if let Some(ref fp) = fp_for_orphans
        && let Ok(gen_id) = ensure_generation_for_fingerprint(store, fp)
    {
        let _ = store.project_resolved_orphan_occurrences(
            gen_id,
            provider.id(),
            provider.model_id(),
            SEMANTIC_SELECTION_VERSION,
        );
    }

    // One owner id per `drive()` call/thread — distinct concurrent
    // `embedding_worker_count` threads each get their own identity so a
    // heartbeat/complete from one can never satisfy another's claim.
    static OWNER_SEQ: AtomicU64 = AtomicU64::new(0);
    let owner = format!(
        "pid{}-drive{}",
        std::process::id(),
        OWNER_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    // Generous relative to EMBED_DEADLINE-class batch latency; a drive loop
    // that outlives this without completing would need heartbeating, which
    // single-shot `drive()` calls (bounded by `cfg.budget_ms`) don't reach.
    const LEASE_MS: i64 = 300_000;

    loop {
        if cancel.is_cancelled() || Instant::now() >= deadline {
            break;
        }
        let mut batch_size = cfg.effective_batch_size();
        if let Some(cap) = oom_batch_cap {
            batch_size = batch_size.min(cap);
        }
        if batch_size == 0 {
            break;
        }
        let claims = store.queue_v2_claim_batch(&owner, LEASE_MS, batch_size)?;
        if claims.is_empty() {
            break;
        }
        // occurrence_id == retrieval_unit_id by construction (one occurrence
        // row per retrieval unit; see `invalidate::reconcile`).
        let token_of: std::collections::HashMap<String, i64> = claims.iter().cloned().collect();
        // [FIX] Everything below this point must NEVER return `Err` out of
        // this loop iteration without first releasing every claimed
        // occurrence back to PENDING/FAILED — a bare `?` here would abandon
        // it INFLIGHT forever (the bug behind observed queue_inflight growth
        // with queue_done stuck at 0, now with a fencing token so any such
        // release cannot race a legitimate concurrent reclaim).

        let target_gen_id = match provider.fingerprint() {
            Some(ref fp) => match ensure_generation_for_fingerprint(store, fp) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("failed to resolve embedding generation: {e}");
                    reset_all(store, &owner, &token_of);
                    continue;
                }
            },
            None => {
                // No stable vector-space identity to commit under. Nothing
                // in v2 is ever enqueued without a fingerprint (see
                // `reconcile`), so this is defensive: release and stop.
                reset_all(store, &owner, &token_of);
                break;
            }
        };

        let occurrences: std::collections::HashMap<String, crate::store::OccurrenceRecord> = {
            let ids: Vec<String> = claims.iter().map(|(id, _)| id.clone()).collect();
            let m = match store.occurrences_by_ids(&ids) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("occurrence lookup failed: {e}");
                    reset_all(store, &owner, &token_of);
                    continue;
                }
            };
            // Claimed but its occurrence row is gone (e.g. the unit was
            // deleted after being queued). Nothing to commit for it —
            // quarantine explicitly instead of leaving it claimable forever.
            for occ_id in &ids {
                if !m.contains_key(occ_id)
                    && let Some(token) = token_of.get(occ_id)
                {
                    let _ = store.queue_v2_fail_permanently(
                        occ_id,
                        &owner,
                        *token,
                        "occurrence record missing",
                    );
                }
            }
            m
        };
        let ids: Vec<String> = occurrences.keys().cloned().collect();
        let rows = match attic_storage::semantic_units_by_ids(conn, &ids) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to load semantic units for batch: {e}");
                reset_all(store, &owner, &token_of);
                continue;
            }
        };

        // Build provider inputs; refuse anything that fails the security
        // gate BEFORE it can reach the provider (§18 defense-in-depth —
        // Phase 1B already redacted retrieval_text upstream).
        let mut inputs: Vec<EmbeddingInput> = Vec::with_capacity(rows.len());
        let mut meta: std::collections::HashMap<String, attic_storage::SemanticUnitRow> =
            std::collections::HashMap::new();
        for r in rows {
            meta.insert(r.unit_id.clone(), r.clone());
            let Some(token) = token_of.get(&r.unit_id).copied() else {
                continue;
            };
            // Scan the CANONICAL text — that is what reaches the provider
            // (r03). retrieval_text may carry pointer/env headers; canonical
            // text is the exact embedded body.
            let scan = secrets::scan_and_redact(&r.canonical_text);
            if !scan.findings.is_empty() {
                tracing::warn!("semantic enrichment refused secret-bearing unit");
                let _ = store.queue_v2_fail_permanently(
                    &r.unit_id,
                    &owner,
                    token,
                    "secret-bearing content",
                );
                stats.skipped_secret += 1;
                continue;
            }
            if r.canonical_text.len() > provider.max_input_bytes() {
                let _ =
                    store.queue_v2_fail_permanently(&r.unit_id, &owner, token, "input too large");
                stats.failed_items += 1;
                continue;
            }
            inputs.push(EmbeddingInput {
                unit_key: r.unit_id.clone(),
                text: r.canonical_text.clone(),
            });
        }
        // [FIX] An occurrence claimed via `ids` that never came back from
        // `semantic_units_by_ids` (e.g. its canonical row was deleted after
        // being queued) was previously left INFLIGHT forever with no error
        // and no resolution. Quarantine it explicitly instead.
        for id in &ids {
            if !meta.contains_key(id)
                && let Some(token) = token_of.get(id)
            {
                let _ =
                    store.queue_v2_fail_permanently(id, &owner, *token, "canonical row missing");
            }
        }

        let mut usage = ResourceUsage::default();
        let plan = crate::cpu_isolation::CpuIsolationPlan::compute(
            cfg.effective_cpu_threads(),
            cfg.embedding_worker_count,
        );

        // Content-addressed reuse: identical canonical body already embedded
        // in THIS vector space gets its stored vector copied instead of
        // re-running inference — the v2 canonical table IS the dedup unit
        // (one row per vector-space + canonical hash, ever), so this is now
        // an exact lookup rather than a best-effort cache. Across a fleet of
        // similar repositories (shared boilerplate, copied components) or a
        // full re-index (fresh unit ids, unchanged bytes) this is the
        // difference between embedding each unique byte sequence once and
        // paying inference again for it.
        let mut handled: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(inputs.len());
        let mut commit_entries: Vec<crate::store::V2CommitEntry> = Vec::with_capacity(inputs.len());
        let mut to_embed: Vec<EmbeddingInput> = Vec::with_capacity(inputs.len());
        let mut hash_of: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for input in inputs {
            let Some(occ) = occurrences.get(&input.unit_key) else {
                continue;
            };
            hash_of.insert(input.unit_key.clone(), occ.canonical_hash.clone());
            match store.embedding_for_canonical(&occ.vector_space_id, &occ.canonical_hash) {
                Ok(Some(vector)) => {
                    handled.insert(input.unit_key.clone());
                    if let Some(r) = meta.get(&input.unit_key) {
                        commit_entries
                            .push(commit_entry(provider, &owner, &token_of, occ, r, vector));
                    }
                }
                Ok(None) => to_embed.push(input),
                Err(e) => {
                    tracing::warn!("canonical embedding lookup failed: {e}");
                    to_embed.push(input);
                }
            }
        }

        // Enrichment's own wall-clock budget is the provider deadline: a
        // slow/hung backend must never hold the drive loop past it.
        // Isolation plan ensures Qwen CPU execution respects orchestrator thread limits.
        let embed_res = if to_embed.is_empty() {
            // Every claimed unit was satisfied by content reuse — no
            // inference to run. Per §11 nothing about cancellation semantics
            // changes: the commit below still either happens whole or not at
            // all.
            Ok(Vec::new())
        } else {
            plan.execute_isolated(|| {
                provider.embed_batch(&to_embed, cancel, &mut usage, Some(deadline))
            })
        };
        match embed_res {
            Ok(outputs) => {
                for out in outputs {
                    if out.vector.len() != provider.dimensions() {
                        return Err(SemanticError::DimensionMismatch {
                            record: out.vector.len(),
                            expected: provider.dimensions(),
                        });
                    }
                    if let (Some(r), Some(occ)) =
                        (meta.get(&out.unit_key), occurrences.get(&out.unit_key))
                    {
                        handled.insert(out.unit_key.clone());
                        commit_entries.push(commit_entry(
                            provider, &owner, &token_of, occ, r, out.vector,
                        ));
                    }
                }
                // [FIX] Any requested input the provider silently dropped
                // (returned fewer vectors than inputs) previously stayed
                // INFLIGHT forever with no error raised anywhere.
                for input in &to_embed {
                    if !handled.contains(&input.unit_key)
                        && let Some(token) = token_of.get(&input.unit_key)
                    {
                        let _ = store.queue_v2_mark_failed(
                            &input.unit_key,
                            &owner,
                            *token,
                            cfg.max_attempts,
                            "provider dropped input",
                        );
                        stats.failed_items += 1;
                    }
                }
                // [FIX] The commit itself is fallible (canonical/semantic DB
                // contention, disk errors). Previously a bare `?` here threw
                // away already-computed embeddings AND left the batch
                // permanently INFLIGHT. Reset on failure so it's retried
                // instead of leaked. Insertion, occurrence completion, and
                // the generation unit-count bump all happen in ONE
                // transaction (`commit_v2_batch`).
                let committed_ids: std::collections::HashSet<String> = commit_entries
                    .iter()
                    .map(|e| e.occurrence_id.clone())
                    .collect();
                match store.commit_v2_batch(
                    &commit_entries,
                    target_gen_id,
                    SEMANTIC_SELECTION_VERSION,
                ) {
                    Ok(committed) => {
                        stats.embedded += committed.len() as u64;
                    }
                    Err(e) => {
                        tracing::warn!("failed to commit embedding batch: {e}");
                        for occ_id in &committed_ids {
                            if let Some(token) = token_of.get(occ_id) {
                                let _ = store.queue_v2_reset(occ_id, &owner, *token);
                            }
                        }
                    }
                }
            }
            Err(SemanticError::Cancelled { .. }) => {
                // Cancellation is NOT failure: by contract the provider
                // commits NOTHING when it reports cancellation, so every
                // item in this batch returns to PENDING untouched and a
                // later drive resumes cleanly (§11).
                stats.cancelled = true;
                reset_all(store, &owner, &token_of);
                break;
            }
            Err(SemanticError::BudgetExhausted(reason)) => {
                // r07: OOM is NOT an item failure. Halve the batch cap
                // (floor 1), return items to PENDING, retry smaller. A
                // single-item OOM is permanent for this content on this
                // device — quarantine it instead of looping forever.
                let current = oom_batch_cap.unwrap_or(batch_size).max(1);
                let next = (current / 2).max(1);
                stats.oom_reductions += 1;
                if claims.len() <= 1 && next == 1 {
                    tracing::warn!(
                        "embedding OOM at single-item batch ({reason}); quarantining item"
                    );
                    fail_all(store, &owner, &token_of, cfg.max_attempts);
                    stats.failed_items += 1;
                } else {
                    tracing::warn!(
                        "embedding OOM ({reason}); batch cap {current} -> {next}, retrying"
                    );
                    oom_batch_cap = Some(next);
                    reset_all(store, &owner, &token_of);
                }
            }
            Err(e) => {
                tracing::warn!("embedding batch failed: {e}");
                fail_all(store, &owner, &token_of, cfg.max_attempts);
                stats.failed_items += token_of.len() as u64;
            }
        }
    }

    if let Some(ref fp) = fp_for_orphans
        && let Ok(gen_id) = ensure_generation_for_fingerprint(store, fp)
    {
        let _ = store.project_resolved_orphan_occurrences(
            gen_id,
            provider.id(),
            provider.model_id(),
            SEMANTIC_SELECTION_VERSION,
        );
    }

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    stats.queue_remaining = store
        .queue_v2_counts()
        .map(|(pending, _, _, _)| pending)
        .unwrap_or(0);
    Ok(stats)
}

/// Fallback drive loop for a provider with no fingerprint — no stable
/// vector-space identity to key v2 occurrences by, so this uses the
/// original v1 unit-keyed queue and per-unit `sem_embeddings` storage
/// directly. Only test doubles (`OomProvider`, `HashingEmbedder`) hit this;
/// every real production provider always returns a fingerprint (Phase 2).
fn drive_v1(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    cfg: &EnrichmentConfig,
    cancel: &CancelFlag,
) -> Result<EnrichStats, SemanticError> {
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(cfg.budget_ms.max(1));
    let mut stats = EnrichStats::default();
    let mut oom_batch_cap: Option<usize> = None;

    loop {
        if cancel.is_cancelled() || Instant::now() >= deadline {
            break;
        }
        let mut batch_size = cfg.effective_batch_size();
        if let Some(cap) = oom_batch_cap {
            batch_size = batch_size.min(cap);
        }
        if batch_size == 0 {
            break;
        }
        let items = store.queue_take_batch(batch_size)?;
        if items.is_empty() {
            break;
        }
        let ids: Vec<String> = items.iter().map(|i| i.retrieval_unit_id.clone()).collect();
        let rows = match attic_storage::semantic_units_by_ids(conn, &ids) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!("failed to load semantic units for batch: {e}");
                for it in &items {
                    store.queue_reset(&it.retrieval_unit_id)?;
                }
                continue;
            }
        };

        let mut inputs: Vec<EmbeddingInput> = Vec::with_capacity(rows.len());
        let mut meta: std::collections::HashMap<String, attic_storage::SemanticUnitRow> =
            std::collections::HashMap::new();
        for r in rows {
            meta.insert(r.unit_id.clone(), r.clone());
            let scan = secrets::scan_and_redact(&r.canonical_text);
            if !scan.findings.is_empty() {
                tracing::warn!("semantic enrichment refused secret-bearing unit");
                store.queue_fail_permanently(&r.unit_id)?;
                stats.skipped_secret += 1;
                continue;
            }
            if r.canonical_text.len() > provider.max_input_bytes() {
                store.queue_fail_permanently(&r.unit_id)?;
                stats.failed_items += 1;
                continue;
            }
            inputs.push(EmbeddingInput {
                unit_key: r.unit_id.clone(),
                text: r.canonical_text.clone(),
            });
        }
        for id in &ids {
            if !meta.contains_key(id) {
                store.queue_fail_permanently(id)?;
            }
        }

        let mut usage = ResourceUsage::default();
        let plan = crate::cpu_isolation::CpuIsolationPlan::compute(
            cfg.effective_cpu_threads(),
            cfg.embedding_worker_count,
        );

        let hashes: Vec<String> = inputs
            .iter()
            .map(|i| crate::identity::content_hash(&i.text))
            .collect();
        let existing = match store.vectors_for_contents(
            provider.id(),
            provider.model_id(),
            provider.dimensions(),
            &hashes,
        ) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("embedding reuse lookup failed: {e}");
                for it in &items {
                    store.queue_reset(&it.retrieval_unit_id)?;
                }
                continue;
            }
        };

        let record_for = |r: &attic_storage::SemanticUnitRow, vector: Vec<f32>| {
            let identity = crate::identity::SemanticUnitIdentity::new(
                r.unit_id.clone(),
                r.source_revision_id.clone(),
                r.index_generation_id.clone(),
                SEMANTIC_SELECTION_VERSION,
                &r.canonical_text,
            );
            crate::store::EmbeddingRecord {
                retrieval_unit_id: identity.retrieval_unit_id,
                repository_id: r.repository_id.clone(),
                source_revision_id: identity.source_revision_id,
                index_generation_id: identity.index_generation_id,
                selection_version: identity.selection_version,
                provider_id: provider.id().to_owned(),
                model_id: provider.model_id().to_owned(),
                content_hash: identity.content_hash,
                dim: vector.len(),
                vector,
            }
        };

        let mut handled: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(inputs.len());
        let mut batch_records: Vec<crate::store::EmbeddingRecord> =
            Vec::with_capacity(inputs.len());
        let mut to_embed: Vec<EmbeddingInput> = Vec::with_capacity(inputs.len());
        for (input, ch) in inputs.into_iter().zip(hashes) {
            match existing.get(&ch) {
                Some(vector) => {
                    if let Some(r) = meta.get(&input.unit_key) {
                        handled.insert(input.unit_key.clone());
                        batch_records.push(record_for(r, vector.clone()));
                    } else {
                        to_embed.push(input);
                    }
                }
                None => to_embed.push(input),
            }
        }

        let embed_res = if to_embed.is_empty() {
            Ok(Vec::new())
        } else {
            plan.execute_isolated(|| {
                provider.embed_batch(&to_embed, cancel, &mut usage, Some(deadline))
            })
        };
        match embed_res {
            Ok(outputs) => {
                for out in outputs {
                    if let Some(r) = meta.get(&out.unit_key) {
                        if out.vector.len() != provider.dimensions() {
                            return Err(SemanticError::DimensionMismatch {
                                record: out.vector.len(),
                                expected: provider.dimensions(),
                            });
                        }
                        handled.insert(out.unit_key.clone());
                        batch_records.push(record_for(r, out.vector));
                    }
                }
                for input in &to_embed {
                    if !handled.contains(&input.unit_key) {
                        store.queue_mark_failed(&input.unit_key, cfg.max_attempts)?;
                        stats.failed_items += 1;
                    }
                }
                match store.put_batch_and_mark_done(&batch_records) {
                    Ok(()) => {
                        stats.embedded += batch_records.len() as u64;
                    }
                    Err(e) => {
                        tracing::warn!("failed to commit embedding batch: {e}");
                        for r in &batch_records {
                            store.queue_reset(&r.retrieval_unit_id)?;
                        }
                    }
                }
            }
            Err(SemanticError::Cancelled { .. }) => {
                stats.cancelled = true;
                for it in &items {
                    store.queue_reset(&it.retrieval_unit_id)?;
                }
                break;
            }
            Err(SemanticError::BudgetExhausted(reason)) => {
                let current = oom_batch_cap.unwrap_or(batch_size).max(1);
                let next = (current / 2).max(1);
                stats.oom_reductions += 1;
                if items.len() <= 1 && next == 1 {
                    tracing::warn!(
                        "embedding OOM at single-item batch ({reason}); quarantining item"
                    );
                    for it in &items {
                        store.queue_mark_failed(&it.retrieval_unit_id, cfg.max_attempts)?;
                        stats.failed_items += 1;
                    }
                } else {
                    tracing::warn!(
                        "embedding OOM ({reason}); batch cap {current} -> {next}, retrying"
                    );
                    oom_batch_cap = Some(next);
                    for it in &items {
                        store.queue_reset(&it.retrieval_unit_id)?;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("embedding batch failed: {e}");
                for it in &items {
                    store.queue_mark_failed(&it.retrieval_unit_id, cfg.max_attempts)?;
                    stats.failed_items += 1;
                }
            }
        }
    }

    stats.elapsed_ms = t0.elapsed().as_millis() as u64;
    stats.queue_remaining = store
        .queue_counts()
        .map(|m| m.get(crate::store::Q_PENDING).copied().unwrap_or(0))
        .unwrap_or(0);
    Ok(stats)
}

/// Release every claimed occurrence back to PENDING without incrementing
/// attempts (cancellation, transient pre-embed failure).
fn reset_all(store: &SemanticStore, owner: &str, token_of: &std::collections::HashMap<String, i64>) {
    for (occ_id, token) in token_of {
        let _ = store.queue_v2_reset(occ_id, owner, *token);
    }
}

/// Record a failed attempt for every claimed occurrence (quarantines as
/// FAILED once `max_attempts` is reached, otherwise back to PENDING).
fn fail_all(
    store: &SemanticStore,
    owner: &str,
    token_of: &std::collections::HashMap<String, i64>,
    max_attempts: u32,
) {
    for (occ_id, token) in token_of {
        let _ = store.queue_v2_mark_failed(occ_id, owner, *token, max_attempts, "");
    }
}

/// Build one commit entry for a resolved (occurrence, unit row, vector)
/// triple — shared by both the content-reuse path and the freshly-embedded
/// path above.
fn commit_entry(
    provider: &dyn SemanticProvider,
    owner: &str,
    token_of: &std::collections::HashMap<String, i64>,
    occ: &crate::store::OccurrenceRecord,
    r: &attic_storage::SemanticUnitRow,
    vector: Vec<f32>,
) -> crate::store::V2CommitEntry {
    crate::store::V2CommitEntry {
        occurrence_id: occ.occurrence_id.clone(),
        owner: owner.to_string(),
        fencing_token: token_of.get(&occ.occurrence_id).copied().unwrap_or(0),
        retrieval_unit_id: occ.retrieval_unit_id.clone(),
        repository_id: r.repository_id.clone(),
        source_revision_id: occ.source_revision_id.clone(),
        index_generation_id: occ.index_generation_id.clone(),
        vector_space_id: occ.vector_space_id.clone(),
        canonical_hash: occ.canonical_hash.clone(),
        provider_id: provider.id().to_owned(),
        model_id: provider.model_id().to_owned(),
        vector,
    }
}

/// Simple bounded background worker (§9): small batches, yields between
/// drives, stops on cancellation. Foreground impact is bounded because the
/// store is the ONLY shared object and queries never lock it.
pub struct BackgroundEnricher {
    stop: std::sync::Arc<CancelFlag>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

/// Shared reconcile-coordination gate (§Phase 8 multi-worker enrichment):
/// with `embedding_worker_count` threads all driving the same queue, only
/// ONE may ever run the real `reconcile()` scan (up to `max_units_total`
/// rows) at a time — every other thread must stay productive pulling
/// embedding work via `queue_take_batch` instead of redundantly reconciling
/// in lockstep. `last_seen_generation`/`last_reconcile_at` are the SAME
/// debounce state the single-threaded version used to keep locally per
/// closure; now shared so the debounce is process-wide, not per-thread.
struct ReconcileGate {
    last_seen_generation: u64,
    last_reconcile_at: Option<Instant>,
    reconciling: bool,
}

/// RAII release for `ReconcileGate::reconciling`: guarantees the flag is
/// cleared even if `reconcile()` panics. Without this, a panic inside
/// `reconcile()` would unwind past a plain `gate.reconciling = false`
/// statement, leaving the flag stuck `true` and permanently blocking every
/// worker's `!gate.reconciling && due_for_reconcile` check for the rest of
/// the process's life.
struct ReconcileGuard<'a>(&'a Mutex<ReconcileGate>);

impl Drop for ReconcileGuard<'_> {
    fn drop(&mut self) {
        let mut gate = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gate.reconciling = false;
    }
}

/// Cheap, self-contained per-call jitter — no randomness crate (`rand`,
/// `fastrand`, ...) is a dependency anywhere in this workspace, so this adds
/// 0-40ms derived from the current subsecond nanosecond count rather than
/// pulling in a new external dependency for a small anti-thundering-herd
/// tweak. Purpose: `embedding_worker_count` threads all backing off on the
/// same fixed intervals would otherwise wake in lockstep and hammer the
/// store/resource-monitor at the same instant.
fn jittered(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    base + Duration::from_millis((nanos % 41) as u64)
}

impl BackgroundEnricher {
    /// Spawn a worker driving the queue with the given cadence. The worker
    /// opens its OWN canonical read connection (rusqlite connections are not
    /// `Sync`, so the pool is never shared across the boundary).
    ///
    /// `resource_monitor`, when present, gates each drive cycle on the same
    /// resource-pressure advisory the incremental scheduler consults (§4/§5:
    /// semantic enrichment is the lowest-priority background subsystem and
    /// must pause under `Pause`/`Emergency` pressure rather than compete with
    /// foreground queries or canonical indexing for memory/CPU).
    ///
    /// `write_generation`: the canonical `WriterQueue`'s commit-generation
    /// counter (`attic_storage::writer::WriterQueueHandle::generation`).
    /// Bumped once per successfully committed canonical write batch — since
    /// EVERY canonical mutation (bootstrap, incremental, watcher-triggered)
    /// is already serialized through that single writer, watching this
    /// counter is a correct, event-driven, zero-cost-when-idle replacement
    /// for polling `reconcile()` on a timer: a plain atomic load per loop
    /// tick, and the (real, up-to-`max_units_total`-row) `reconcile()` scan
    /// only runs when something has actually changed since it last ran.
    ///
    /// [FIX] `RECONCILE_MIN_INTERVAL` debounces the trigger itself: during
    /// active bulk indexing the writer commits constantly, so the counter
    /// changes on nearly every loop tick — without a floor, `reconcile()`
    /// (a real scan of up to `max_units_total` rows) would fire back-to-back
    /// precisely during the highest-load moment (large multi-repo indexing),
    /// competing with the canonical writer for I/O/CPU instead of staying
    /// out of its way. Reacting to the counter (not a blind timer) still
    /// keeps the idle case free; the floor bounds the busy case.
    ///
    /// [FIX] `cfg.embedding_worker_count` worker threads are spawned (rather
    /// than exactly one), each racing to pull batches off the same queue via
    /// `queue_take_batch` (now atomic — see that function's doc comment).
    /// `reconcile()` itself must NOT run concurrently from multiple threads,
    /// so its debounce state (`last_seen_generation`/`last_reconcile_at`,
    /// plus a new `reconciling` flag) moved out of each thread's local
    /// closure into one shared `Arc<Mutex<ReconcileGate>>` constructed here
    /// and cloned into every thread: whichever thread wins the gate check
    /// runs `reconcile()`; every other thread that tick skips straight to
    /// `drive()` so all threads stay productive on embedding work.
    pub fn spawn(
        canonical_db_path: std::path::PathBuf,
        store: std::sync::Arc<SemanticStore>,
        provider: std::sync::Arc<dyn SemanticProvider>,
        cfg: EnrichmentConfig,
        resource_monitor: Option<std::sync::Arc<attic_storage::resource_manager::ResourceMonitor>>,
        write_generation: Arc<AtomicU64>,
    ) -> Self {
        let stop = std::sync::Arc::new(CancelFlag::new());
        // Floor between actual `reconcile()` scans, regardless of how often
        // the generation counter changes in between — see the `[FIX]` note
        // on `spawn`'s doc comment above.
        const RECONCILE_MIN_INTERVAL: Duration = Duration::from_secs(2);
        // Seeded to force a mismatch on the very first tick, so a freshly
        // (re)started server always reconciles once up front — covers
        // "already-indexed-but-never-embedded" content from before this
        // worker existed or from a restart.
        let initial_generation = write_generation.load(Ordering::Acquire).wrapping_sub(1);
        let reconcile_gate = Arc::new(Mutex::new(ReconcileGate {
            last_seen_generation: initial_generation,
            last_reconcile_at: None,
            reconciling: false,
        }));

        // Honor the provider's real inference concurrency before spawning
        // workers or claiming queue rows. Qwen owns one mutex-protected model,
        // so spawning eight callers merely left seven blocked on that mutex,
        // inflated INFLIGHT by 7 * batch_size, and (via CpuIsolationPlan)
        // divided the usable CPU budget among lanes that never ran.
        let worker_count = provider
            .concurrency_contract()
            .effective_workers(cfg.embedding_worker_count);
        // `drive()` uses this count to divide its CPU allocation. Store the
        // effective count, not the requested count, so a serialized provider
        // receives the full semantic CPU grant in its sole runnable lane.
        let cfg = EnrichmentConfig {
            embedding_worker_count: worker_count,
            ..cfg
        };
        let mut handles = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let stop2 = stop.clone();
            let conn_path = canonical_db_path.clone();
            let store = store.clone();
            let provider = provider.clone();
            let cfg = cfg.clone();
            let resource_monitor = resource_monitor.clone();
            let write_generation = write_generation.clone();
            let reconcile_gate = reconcile_gate.clone();
            let handle = std::thread::spawn(move || {
                // [FIX] Use the shared pragma-configured opener (WAL +
                // busy_timeout=5000, etc.) instead of a raw `Connection::open`.
                // A bare connection has no busy_timeout, so any transient lock
                // held by the canonical writer (bootstrap/incremental commits)
                // surfaced as an immediate SQLITE_BUSY error here — which,
                // combined with drive()'s per-batch error handling, silently
                // abandoned the whole already-claimed INFLIGHT batch.
                let conn = match attic_storage::connection::open_ro(&conn_path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("background enrichment cannot open index: {e}");
                        return;
                    }
                };
                while !stop2.is_cancelled() {
                    if let Some(monitor) = resource_monitor.as_ref() {
                        use attic_storage::resource_manager::{ResourceAdvisory, current_advisory};
                        if matches!(current_advisory(monitor), ResourceAdvisory::Restricted) {
                            std::thread::sleep(jittered(Duration::from_millis(200)));
                            continue;
                        }
                    }
                    let current_generation = write_generation.load(Ordering::Acquire);
                    // Claim the reconcile gate (if due and not already held)
                    // under the shared lock, then release it BEFORE actually
                    // calling reconcile() — never call out to reconcile()
                    // while holding the gate's mutex.
                    let should_reconcile = {
                        let mut gate = reconcile_gate
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        let due_for_reconcile = match gate.last_reconcile_at {
                            Some(t) => t.elapsed() >= RECONCILE_MIN_INTERVAL,
                            None => true,
                        };
                        if !gate.reconciling
                            && due_for_reconcile
                            && current_generation != gate.last_seen_generation
                        {
                            gate.reconciling = true;
                            gate.last_seen_generation = current_generation;
                            gate.last_reconcile_at = Some(Instant::now());
                            true
                        } else {
                            false
                        }
                    };
                    if should_reconcile {
                        let _release_gate = ReconcileGuard(&reconcile_gate);
                        match reconcile(&conn, &store, provider.as_ref(), &cfg.selection) {
                            Ok(report) if report.enqueued > 0 => {
                                tracing::info!(
                                    enqueued = report.enqueued,
                                    invalidated = report.invalidated_stale,
                                    "semantic reconcile"
                                );
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("semantic reconcile failed: {e}"),
                        }
                        // _release_gate drops here (and on any unwind out of
                        // the match above), clearing `reconciling` exactly
                        // once either way.
                    }
                    // ── Phase 3/8: adaptive embedding admission ──────────────
                    // Acquire an EmbeddingHeavyPermit before the expensive
                    // model/batch execution phase; read the dynamic batch size
                    // at the point of each new batch so pressure reductions
                    // take effect immediately rather than only on the next
                    // server restart. The permit is held for the duration of
                    // `drive()` and released on drop.
                    //
                    // If the resource monitor reports Emergency (no new
                    // permits available) `acquire_embedding_heavy_blocking`
                    // returns `None` — loop back and sleep rather than
                    // skipping the advisory check entirely.
                    let _embed_permit;
                    let effective_cfg;
                    let drive_cfg: &EnrichmentConfig = if let Some(monitor) =
                        resource_monitor.as_ref()
                    {
                        let dynamic_batch = monitor.current_embedding_batch();
                        match monitor.acquire_embedding_heavy_blocking(|| stop2.is_cancelled()) {
                            Some(permit) => {
                                _embed_permit = Some(permit);
                                effective_cfg = EnrichmentConfig {
                                    batch_size: dynamic_batch,
                                    ..cfg.clone()
                                };
                                &effective_cfg
                            }
                            None => {
                                // Cancelled (stop2) or Emergency — sleep
                                // and retry rather than driving with no
                                // permit.
                                _embed_permit = None;
                                std::thread::sleep(jittered(Duration::from_millis(200)));
                                continue;
                            }
                        }
                    } else {
                        // No resource monitor (tests / no-daemon mode) —
                        // use the static config unchanged.
                        _embed_permit = None;
                        effective_cfg = cfg.clone();
                        &effective_cfg
                    };

                    match drive(&conn, &store, provider.as_ref(), drive_cfg, &stop2) {
                        Ok(s) if s.embedded == 0 && !s.cancelled => {
                            // Queue drained; idle-poll so we stay responsive to
                            // new enqueues without spinning hot.
                            std::thread::sleep(jittered(Duration::from_millis(50)));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("background enrichment error: {e}");
                            std::thread::sleep(jittered(Duration::from_millis(200)));
                        }
                    }
                }
            });
            handles.push(handle);
        }
        Self { stop, handles }
    }

    /// Spawn background enrichment workers wired directly to the ResourceOrchestrator (§12, §15, CP15).
    pub fn spawn_with_orchestrator(
        canonical_db_path: std::path::PathBuf,
        store: std::sync::Arc<SemanticStore>,
        provider: std::sync::Arc<dyn SemanticProvider>,
        mut cfg: EnrichmentConfig,
        resource_monitor: Option<std::sync::Arc<attic_storage::resource_manager::ResourceMonitor>>,
        write_generation: Arc<AtomicU64>,
        orchestrator: &attic_storage::ResourceOrchestrator,
    ) -> Self {
        cfg.dynamic_allocation = Some(orchestrator.shared_allocation());
        Self::spawn(
            canonical_db_path,
            store,
            provider,
            cfg,
            resource_monitor,
            write_generation,
        )
    }

    /// Request stop and join every worker thread against a SHARED timeout
    /// budget; true only when ALL of them exited within it (matching the
    /// original single-handle contract, generalized to N handles).
    pub fn shutdown(mut self, timeout: Duration) -> bool {
        self.stop.cancel();
        let deadline = Instant::now() + timeout;
        let mut all_joined = true;
        for h in self.handles.drain(..) {
            let mut joined = false;
            while Instant::now() < deadline {
                if h.is_finished() {
                    let _ = h.join();
                    joined = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            if !joined {
                all_joined = false; // deterministic timeout; test owns cleanup decisions
            }
        }
        all_joined
    }
}

#[cfg(test)]
mod generation_driven_enrichment_tests {
    use super::*;
    use crate::provider::ExecutionBackend;

    #[test]
    fn worker_count_honors_provider_concurrency_contract() {
        use crate::provider::ProviderConcurrencyContract;

        assert_eq!(
            ProviderConcurrencyContract::Serialized.effective_workers(8),
            1
        );
        assert_eq!(
            ProviderConcurrencyContract::SharedConcurrent.effective_workers(8),
            8
        );
        assert_eq!(
            ProviderConcurrencyContract::PooledLanes { max_lanes: 3 }.effective_workers(8),
            3
        );
        assert_eq!(
            ProviderConcurrencyContract::PooledLanes { max_lanes: 0 }.effective_workers(0),
            1
        );
    }

    fn test_fp(model: &str) -> EmbeddingFingerprint {
        EmbeddingFingerprint {
            provider: "qwen3".to_string(),
            model_id: model.to_string(),
            model_revision: "rev1".to_string(),
            dimension: 512,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "tok_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_retrieval_v1".to_string(),
            execution_backend: ExecutionBackend::Unknown,
            quantization: "test-none".to_string(),
        }
    }

    #[test]
    fn initial_fingerprint_creates_and_activates_generation() {
        let store = SemanticStore::open_in_memory().unwrap();
        let fp = test_fp("qwen3-0.6b");
        let gen_id = ensure_generation_for_fingerprint(&store, &fp).unwrap();
        assert_eq!(gen_id, 1);
        let active = store.get_active_generation().unwrap().unwrap();
        assert_eq!(active.generation_id, 1);
        assert_eq!(active.fingerprint, fp);
    }

    #[test]
    fn matching_fingerprint_reuses_active_generation() {
        let store = SemanticStore::open_in_memory().unwrap();
        let fp = test_fp("qwen3-0.6b");
        let gen_id1 = ensure_generation_for_fingerprint(&store, &fp).unwrap();
        let gen_id2 = ensure_generation_for_fingerprint(&store, &fp).unwrap();
        assert_eq!(gen_id1, gen_id2);
    }

    #[test]
    fn differing_fingerprint_starts_building_generation_without_disturbing_active() {
        let store = SemanticStore::open_in_memory().unwrap();
        let fp1 = test_fp("qwen3-0.6b");
        let gen1 = ensure_generation_for_fingerprint(&store, &fp1).unwrap();
        assert_eq!(gen1, 1);

        let fp2 = test_fp("qwen3-1.5b");
        let gen2 = ensure_generation_for_fingerprint(&store, &fp2).unwrap();
        assert_eq!(gen2, 2);

        // Active generation must still be Gen 1
        let active = store.get_active_generation().unwrap().unwrap();
        assert_eq!(active.generation_id, 1);
        assert_eq!(active.fingerprint, fp1);

        // Building generation must be Gen 2
        let building = store.get_building_generation().unwrap().unwrap();
        assert_eq!(building.generation_id, 2);
        assert_eq!(building.fingerprint, fp2);
    }

    #[test]
    fn effective_batch_size_obeys_dynamic_allocation_and_clamps() {
        use std::sync::{Arc, RwLock};

        let mut cfg = EnrichmentConfig {
            batch_size: 16,
            ..EnrichmentConfig::default()
        };
        assert_eq!(cfg.effective_batch_size(), 16);

        let alloc = Arc::new(RwLock::new(attic_storage::ResourceAllocation {
            semantic_batch_size: 32,
            ..Default::default()
        }));

        cfg.dynamic_allocation = Some(alloc.clone());
        // Dynamic batch is 32, but cfg.batch_size is 16 (e.g. from ResourceMonitor clamp),
        // so min(32, 16) = 16.
        assert_eq!(cfg.effective_batch_size(), 16);

        // If cfg.batch_size is higher (e.g. 64), then dynamic allocation of 32 limits it to 32.
        cfg.batch_size = 64;
        assert_eq!(cfg.effective_batch_size(), 32);

        // If dynamic allocation sets semantic_batch_size to 0 (emergency halt), effective is 0.
        alloc.write().unwrap().semantic_batch_size = 0;
        assert_eq!(cfg.effective_batch_size(), 0);
    }

    /// r07: an OOM (BudgetExhausted) batch must NOT fail items — the drive
    /// halves the batch cap, returns items to PENDING, and completes them at
    /// the reduced size.
    #[test]
    fn oom_batch_halves_and_eventually_embeds_everything() {
        use attic_core::{
            DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
            IndexGenerationId, RepositoryId, SecurityState, SourceRevisionId, SourceType,
            SubsystemVersions,
        };

        // Canonical in-memory DB with four indexable units.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        attic_storage::run_migrations(&conn).unwrap();
        let repo_id = RepositoryId::new_v4();
        attic_storage::upsert_repository(&conn, &repo_id, "/repo/oom", "oom-repo").unwrap();
        let rev_id = SourceRevisionId::new_v4();
        attic_storage::insert_source_revision(
            &conn,
            &rev_id,
            &repo_id,
            "abc123",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        let gen_id = IndexGenerationId::new_v4();
        attic_storage::insert_index_generation(
            &conn,
            &gen_id,
            &repo_id,
            &rev_id,
            1,
            &SubsystemVersions::new(),
        )
        .unwrap();
        let fid = FileIdentityId::new_v4();
        attic_storage::upsert_file_identity(&conn, &fid, &repo_id, "basis").unwrap();
        let occ_id = FileOccurrenceId::new_v4();
        attic_storage::insert_file_occurrence(
            &conn,
            &attic_storage::NewFileOccurrence {
                id: &occ_id,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: Some(&gen_id),
                path: "src/lib.rs",
                content_hash: "blake3:aa",
                size_bytes: 128,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Clean,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();

        let store = SemanticStore::open_in_memory().unwrap();
        let mut unit_ids = Vec::new();
        for i in 0..4 {
            let unit_id = attic_core::RetrievalUnitId::new_v4().to_string_repr();
            attic_storage::insert_retrieval_unit_with_fts(
                &conn,
                &attic_storage::NewRetrievalUnit {
                    id: &unit_id,
                    file_occurrence_id: &occ_id.to_string_repr(),
                    index_generation_id: &gen_id.to_string_repr(),
                    repository_id: &repo_id.to_string_repr(),
                    retrieval_text: match i {
                        0 => "fn oom_token_alpha() {}",
                        1 => "fn oom_token_beta() {}",
                        2 => "fn oom_token_gamma() {}",
                        _ => "fn oom_token_delta() {}",
                    },
                    analyzer_id: "generic",
                    analyzer_version: "test",
                    start_line: Some(i as u32),
                    end_line: Some(i as u32),
                    is_redacted: false,
                },
            )
            .unwrap();
            unit_ids.push(unit_id);
        }
        store.queue_enqueue(&unit_ids, 0.5).unwrap();

        // Batch 4 requested; provider OOMs above 2.
        let provider = crate::testing::OomProvider { max_items: 2 };
        let cfg = EnrichmentConfig {
            batch_size: 4,
            budget_ms: 30_000,
            ..EnrichmentConfig::default()
        };
        let stats = drive(&conn, &store, &provider, &cfg, &CancelFlag::new()).unwrap();
        assert_eq!(stats.embedded, 4, "all four units embed after halving");
        assert_eq!(stats.oom_reductions, 1, "exactly one halving event");
        assert_eq!(stats.failed_items, 0);
        assert_eq!(stats.queue_remaining, 0);
    }
}
