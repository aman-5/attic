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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use attic_discovery::secrets;
use rusqlite::Connection;

use crate::embedding_profile::{ClaimOutcome, EmbeddingIntentSource};
use crate::error::SemanticError;
use crate::identity::SemanticUnitIdentity;
use crate::invalidate::reconcile;
use crate::provider::{CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider};
use crate::selection::{SEMANTIC_SELECTION_VERSION, SelectionConfig};
use crate::store::{EmbeddingRecord, SemanticStore};

/// Claim/verify this provider's `EmbeddingProfile` before the FIRST real
/// embedding work in this store's lifetime (Low-Level Design §3) — never at
/// startup or on a mere `status` check. `provider.embedding_descriptor()`
/// returning `None` (the baseline `HashingEmbedder`, test doubles) means
/// this provider has no persisted-identity concept; profile claiming is
/// skipped entirely for it, unchanged from today's behavior.
///
/// `claim_embedding_profile_if_absent` is idempotent (`ON CONFLICT DO
/// NOTHING`), so calling this once per non-empty drive iteration is cheap
/// and safe — it only ever performs a real write the very first time.
///
/// Returns `true` when it is safe to proceed with this batch, `false` on a
/// genuine `Conflict` (this provider's identity lost a first-claim race, or
/// disagrees with an already-persisted profile). KNOWN LIMITATION, flagged
/// honestly rather than silently handled: on `Conflict` this process does
/// NOT hot-swap to the winning provider — it simply stops embedding and
/// leaves the batch `PENDING` for a future restart, which re-resolves the
/// correct provider from the persisted profile at startup (see
/// `attic-server`'s `new_with_semantic_opt`). Self-healing across a restart,
/// not mid-process — acceptable because this is a narrow, transient race
/// between two cold-starting processes, not the steady-state path.
///
/// SECOND KNOWN LIMITATION on the `AdoptedRace` branch specifically: since
/// this process's `SemanticProvider` was already constructed at startup
/// (before any claim happens — see `resolve_semantic_provider`), "adopt the
/// winner" here does NOT hot-swap the provider instance either; it only
/// means "the DB row now reflects the winner, and this call still returns
/// `true`." This process's embeddings continue to be tagged with ITS OWN
/// actual `provider.id()`/`model_id()` (never a false identity — `store.put`
/// always uses the real computing provider's own strings), so there is no
/// silent corruption. The narrow real consequence is index fragmentation:
/// in the rare window where two cold-starting processes raced with
/// different (but both merely-recommended) providers, each keeps embedding
/// under its own tag until a restart converges both onto the persisted
/// winner. Accepted as out of scope for the same reason as the `Conflict`
/// case — a transient race, not the steady-state path.
fn ensure_profile_claimed(
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    intent_source: EmbeddingIntentSource,
) -> Result<bool, SemanticError> {
    let Some(descriptor) = provider.embedding_descriptor() else {
        return Ok(true);
    };
    match store.claim_embedding_profile_if_absent(descriptor, intent_source)? {
        ClaimOutcome::Claimed(p) => {
            tracing::info!(profile_id = %p.id, "claimed embedding profile at first real indexing work");
            Ok(true)
        }
        ClaimOutcome::ExistingMatched(_) | ClaimOutcome::AdoptedRace { .. } => Ok(true),
        ClaimOutcome::Conflict { requested, adopted } => {
            tracing::warn!(
                requested_model = %requested.model,
                adopted_model = %adopted.config.model,
                "embedding profile conflict: this process's provider does not match the \
                 persisted profile; pausing enrichment until restart (re-index required)"
            );
            Ok(false)
        }
    }
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
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            batch_size: 16,
            max_attempts: 3,
            budget_ms: 2_000,
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
    /// [FIX] Set when this drive stopped early because
    /// `ensure_profile_claimed` hit a `Conflict` — the batch was reset to
    /// `PENDING`, not drained. Only a restart can resolve this (see
    /// `ensure_profile_claimed`'s docs), so `BackgroundEnricher` backs off
    /// far longer than its normal idle-poll interval when this is set,
    /// instead of retrying the same doomed check every ~50ms forever.
    pub blocked_by_conflict: bool,
}

/// Drive the enrichment queue until empty or budget/cancellation bounds hit.
///
/// `conn` is a CANONICAL READ-ONLY connection; nothing here writes to the
/// canonical database.
pub fn drive(
    conn: &Connection,
    store: &SemanticStore,
    provider: &dyn SemanticProvider,
    cfg: &EnrichmentConfig,
    cancel: &CancelFlag,
    intent_source: EmbeddingIntentSource,
) -> Result<EnrichStats, SemanticError> {
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_millis(cfg.budget_ms.max(1));
    let mut stats = EnrichStats::default();

    loop {
        if cancel.is_cancelled() || Instant::now() >= deadline {
            break;
        }
        let items = store.queue_take_batch(cfg.batch_size)?;
        if items.is_empty() {
            break;
        }
        if !ensure_profile_claimed(store, provider, intent_source)? {
            // Conflict: leave this batch PENDING and stop driving for now —
            // see `ensure_profile_claimed`'s docs for why this self-heals on
            // restart rather than hot-swapping providers mid-process.
            for it in &items {
                store.queue_reset(&it.retrieval_unit_id)?;
            }
            stats.blocked_by_conflict = true;
            break;
        }
        let ids: Vec<String> = items.iter().map(|i| i.retrieval_unit_id.clone()).collect();
        let rows = attic_storage::semantic_units_by_ids(conn, &ids)?;

        // Build provider inputs; refuse anything that fails the security
        // gate BEFORE it can reach the provider (§18 defense-in-depth —
        // Phase 1B already redacted retrieval_text upstream).
        let mut inputs: Vec<EmbeddingInput> = Vec::with_capacity(rows.len());
        let mut meta: std::collections::HashMap<String, attic_storage::SemanticUnitRow> =
            std::collections::HashMap::new();
        for r in rows {
            meta.insert(r.unit_id.clone(), r.clone());
            let scan = secrets::scan_and_redact(&r.retrieval_text);
            if !scan.findings.is_empty() {
                tracing::warn!("semantic enrichment refused secret-bearing unit");
                store.queue_fail_permanently(&r.unit_id)?;
                stats.skipped_secret += 1;
                continue;
            }
            if r.retrieval_text.len() > provider.max_input_bytes() {
                store.queue_fail_permanently(&r.unit_id)?;
                stats.failed_items += 1;
                continue;
            }
            inputs.push(EmbeddingInput {
                unit_key: r.unit_id.clone(),
                text: r.retrieval_text.clone(),
            });
        }

        let mut usage = ResourceUsage::default();
        // Enrichment's own wall-clock budget is the provider deadline: a
        // slow/hung backend must never hold the drive loop past it.
        match provider.embed_batch(&inputs, cancel, &mut usage, Some(deadline)) {
            Ok(outputs) => {
                for out in outputs {
                    if let Some(r) = meta.get(&out.unit_key) {
                        if out.vector.len() != provider.dimensions() {
                            return Err(SemanticError::DimensionMismatch {
                                record: out.vector.len(),
                                expected: provider.dimensions(),
                            });
                        }
                        let identity = SemanticUnitIdentity::new(
                            r.unit_id.clone(),
                            r.source_revision_id.clone(),
                            r.index_generation_id.clone(),
                            SEMANTIC_SELECTION_VERSION,
                            &r.retrieval_text,
                        );
                        store.put(&EmbeddingRecord {
                            retrieval_unit_id: identity.retrieval_unit_id,
                            repository_id: r.repository_id.clone(),
                            source_revision_id: identity.source_revision_id,
                            index_generation_id: identity.index_generation_id,
                            selection_version: identity.selection_version,
                            provider_id: provider.id().to_owned(),
                            model_id: provider.model_id().to_owned(),
                            content_hash: identity.content_hash,
                            dim: out.vector.len(),
                            vector: out.vector,
                        })?;
                        store.queue_mark_done(&out.unit_key)?;
                        stats.embedded += 1;
                    }
                }
            }
            Err(SemanticError::Cancelled { .. }) => {
                // Cancellation is NOT failure: by contract the provider
                // commits NOTHING when it reports cancellation, so every
                // item in this batch returns to PENDING untouched and a
                // later drive resumes cleanly (§11).
                stats.cancelled = true;
                for it in &items {
                    store.queue_reset(&it.retrieval_unit_id)?;
                }
                break;
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

/// Simple bounded background worker (§9): small batches, yields between
/// drives, stops on cancellation. Foreground impact is bounded because the
/// store is the ONLY shared object and queries never lock it.
pub struct BackgroundEnricher {
    stop: std::sync::Arc<CancelFlag>,
    handle: Option<std::thread::JoinHandle<()>>,
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
    pub fn spawn(
        canonical_db_path: std::path::PathBuf,
        store: std::sync::Arc<SemanticStore>,
        provider: std::sync::Arc<dyn SemanticProvider>,
        cfg: EnrichmentConfig,
        resource_monitor: Option<std::sync::Arc<attic_storage::resource_manager::ResourceMonitor>>,
        intent_source: EmbeddingIntentSource,
        write_generation: Arc<AtomicU64>,
    ) -> Self {
        let stop = std::sync::Arc::new(CancelFlag::new());
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let conn = match Connection::open(&canonical_db_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("background enrichment cannot open index: {e}");
                    return;
                }
            };
            // Seeded to force a mismatch on the very first tick, so a
            // freshly (re)started server always reconciles once up front —
            // covers "already-indexed-but-never-embedded" content from
            // before this worker existed or from a restart.
            let mut last_seen_generation = write_generation.load(Ordering::Acquire).wrapping_sub(1);
            // Floor between actual `reconcile()` scans, regardless of how
            // often the generation counter changes in between — see the
            // `[FIX]` note on `spawn`'s doc comment above.
            const RECONCILE_MIN_INTERVAL: Duration = Duration::from_secs(2);
            let mut last_reconcile_at: Option<Instant> = None;
            while !stop2.is_cancelled() {
                if let Some(monitor) = resource_monitor.as_ref() {
                    use attic_storage::resource_manager::{ResourceAdvisory, current_advisory};
                    if matches!(
                        current_advisory(monitor),
                        ResourceAdvisory::Pause | ResourceAdvisory::Emergency
                    ) {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                }
                let current_generation = write_generation.load(Ordering::Acquire);
                let due_for_reconcile = match last_reconcile_at {
                    Some(t) => t.elapsed() >= RECONCILE_MIN_INTERVAL,
                    None => true,
                };
                if current_generation != last_seen_generation && due_for_reconcile {
                    match reconcile(
                        &conn,
                        &store,
                        provider.as_ref(),
                        &SelectionConfig::default(),
                    ) {
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
                    last_seen_generation = current_generation;
                    last_reconcile_at = Some(Instant::now());
                }
                match drive(
                    &conn,
                    &store,
                    provider.as_ref(),
                    &cfg,
                    &stop2,
                    intent_source,
                ) {
                    // [FIX] A Conflict can only ever be resolved by a
                    // restart (see ensure_profile_claimed's docs) — retrying
                    // the same doomed claim check every ~50ms forever just
                    // burns DB round trips for no possible gain. Back off far
                    // longer; still cooperatively cancellable via `stop2`.
                    Ok(s) if s.blocked_by_conflict => {
                        tracing::warn!(
                            "semantic enrichment blocked by an embedding-profile conflict; \
                             backing off until restart (see status for re_index_recommended)"
                        );
                        let backoff = Duration::from_secs(30);
                        let deadline = Instant::now() + backoff;
                        while Instant::now() < deadline && !stop2.is_cancelled() {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                    }
                    Ok(s) if s.embedded == 0 && !s.cancelled => {
                        // Queue drained; idle-poll so we stay responsive to
                        // new enqueues without spinning hot.
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("background enrichment error: {e}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Request stop and join with timeout; true when the worker exited.
    pub fn shutdown(mut self, timeout: Duration) -> bool {
        self.stop.cancel();
        match self.handle.take() {
            Some(h) => {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if h.is_finished() {
                        let _ = h.join();
                        return true;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                false // deterministic timeout; test owns cleanup decisions
            }
            None => true,
        }
    }
}

#[cfg(test)]
mod ensure_profile_claimed_tests {
    use super::*;
    use crate::embedding_profile::{EmbeddingSpaceDescriptor, PoolingStrategy, TruncationPolicy};
    use crate::provider::{CancelFlag, EmbeddingOutput};

    /// Minimal provider whose only purpose is returning a fixed
    /// `EmbeddingSpaceDescriptor` — exercises `ensure_profile_claimed`
    /// without needing a real `BgeEmbedder` (network/model weights).
    struct DescriptorProvider(EmbeddingSpaceDescriptor);

    impl SemanticProvider for DescriptorProvider {
        fn id(&self) -> &'static str {
            "descriptor-test"
        }
        fn model_id(&self) -> &str {
            "descriptor-test-v1"
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn max_input_bytes(&self) -> usize {
            4096
        }
        fn embedding_descriptor(&self) -> Option<EmbeddingSpaceDescriptor> {
            Some(self.0.clone())
        }
        fn embed_batch(
            &self,
            _inputs: &[EmbeddingInput],
            _cancel: &CancelFlag,
            _usage: &mut ResourceUsage,
            _deadline: Option<Instant>,
        ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
            Ok(vec![])
        }
    }

    fn descriptor(model: &str) -> EmbeddingSpaceDescriptor {
        EmbeddingSpaceDescriptor {
            schema_version: EmbeddingSpaceDescriptor::SCHEMA_VERSION,
            provider: "bge".into(),
            model: model.into(),
            model_revision: "rev1".into(),
            tokenizer_revision: "rev1".into(),
            pooling: PoolingStrategy::Cls,
            normalize: true,
            truncation: TruncationPolicy::Truncate,
            max_tokens: 512,
        }
    }

    #[test]
    fn provider_with_no_descriptor_never_claims_anything() {
        let store = SemanticStore::open_in_memory().unwrap();
        let provider = crate::providers::HashingEmbedder::new();
        assert!(
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::Recommendation)
                .unwrap()
        );
        assert!(store.read_embedding_profile().unwrap().is_none());
    }

    #[test]
    fn first_real_work_claims_the_profile() {
        let store = SemanticStore::open_in_memory().unwrap();
        let provider = DescriptorProvider(descriptor("bge-small-en-v1.5"));
        assert!(
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::Recommendation)
                .unwrap()
        );
        let persisted = store.read_embedding_profile().unwrap().unwrap();
        assert_eq!(persisted.config.model, "bge-small-en-v1.5");
    }

    #[test]
    fn matching_persisted_profile_proceeds() {
        let store = SemanticStore::open_in_memory().unwrap();
        let provider = DescriptorProvider(descriptor("bge-small-en-v1.5"));
        assert!(
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::Recommendation)
                .unwrap()
        );
        // Second call (as if a later drive() iteration) — idempotent, still proceeds.
        assert!(
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::Recommendation)
                .unwrap()
        );
    }

    #[test]
    fn conflicting_explicit_provider_refuses_to_embed() {
        let store = SemanticStore::open_in_memory().unwrap();
        // A different process already claimed "bge-small-en-v1.5".
        store
            .claim_embedding_profile_if_absent(
                descriptor("bge-small-en-v1.5"),
                EmbeddingIntentSource::Recommendation,
            )
            .unwrap();
        // This process's provider is explicitly configured for a different model.
        let provider = DescriptorProvider(descriptor("bge-large-en-v1.5"));
        let proceed =
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::TomlOverride).unwrap();
        assert!(
            !proceed,
            "an explicit-intent conflict must refuse to embed under the wrong identity"
        );
        // The persisted profile must remain the original — never silently overwritten.
        let persisted = store.read_embedding_profile().unwrap().unwrap();
        assert_eq!(persisted.config.model, "bge-small-en-v1.5");
    }

    #[test]
    fn conflicting_recommendation_only_adopts_the_winner_and_proceeds() {
        let store = SemanticStore::open_in_memory().unwrap();
        store
            .claim_embedding_profile_if_absent(
                descriptor("bge-small-en-v1.5"),
                EmbeddingIntentSource::Recommendation,
            )
            .unwrap();
        // This process's own descriptor differs but was only ever a
        // recommendation (no explicit user intent) — safe to proceed. NOTE:
        // this does NOT hot-swap the provider instance (see
        // ensure_profile_claimed's doc comment, "SECOND KNOWN LIMITATION") —
        // it only verifies the call returns true rather than refusing.
        let provider = DescriptorProvider(descriptor("bge-large-en-v1.5"));
        let proceed =
            ensure_profile_claimed(&store, &provider, EmbeddingIntentSource::Recommendation)
                .unwrap();
        assert!(
            proceed,
            "a recommendation-only mismatch must not refuse to proceed"
        );
    }
}
