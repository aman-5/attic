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

use std::time::{Duration, Instant};

use attic_discovery::secrets;
use rusqlite::Connection;

use crate::error::SemanticError;
use crate::identity::SemanticUnitIdentity;
use crate::provider::{CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider};
use crate::selection::SEMANTIC_SELECTION_VERSION;
use crate::store::{EmbeddingRecord, SemanticStore};

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
        match provider.embed_batch(&inputs, cancel, &mut usage) {
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
    pub fn spawn(
        canonical_db_path: std::path::PathBuf,
        store: std::sync::Arc<SemanticStore>,
        provider: std::sync::Arc<dyn SemanticProvider>,
        cfg: EnrichmentConfig,
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
            while !stop2.is_cancelled() {
                match drive(&conn, &store, provider.as_ref(), &cfg, &stop2) {
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
