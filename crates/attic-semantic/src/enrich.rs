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
use crate::identity::SemanticUnitIdentity;
use crate::invalidate::reconcile;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, ResourceUsage, SemanticProvider,
};
use crate::selection::{SEMANTIC_SELECTION_VERSION, SelectionConfig};
use crate::store::{EmbeddingRecord, SemanticStore};

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
    /// Optional dynamic resource allocation handle from ResourceOrchestrator (Master Plan §12, §15, CP15).
    pub dynamic_allocation: Option<Arc<std::sync::RwLock<attic_storage::ResourceAllocation>>>,
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
            dynamic_allocation: None,
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
        2
    }
}

impl Default for EnrichmentConfig {
    fn default() -> Self {
        Self {
            batch_size: 16,
            max_attempts: 3,
            budget_ms: 2_000,
            embedding_worker_count: 1,
            dynamic_allocation: None,
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
        let batch_size = cfg.effective_batch_size();
        if batch_size == 0 {
            break;
        }
        let items = store.queue_take_batch(batch_size)?;
        if items.is_empty() {
            break;
        }
        let target_gen_id = match provider.fingerprint() {
            Some(ref fp) => Some(ensure_generation_for_fingerprint(store, fp)?),
            None => None,
        };
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
        let plan = crate::cpu_isolation::CpuIsolationPlan::compute(
            cfg.effective_cpu_threads(),
            cfg.embedding_worker_count,
        );
        // Enrichment's own wall-clock budget is the provider deadline: a
        // slow/hung backend must never hold the drive loop past it.
        // Isolation plan ensures Qwen CPU execution respects orchestrator thread limits.
        let embed_res = plan
            .execute_isolated(|| provider.embed_batch(&inputs, cancel, &mut usage, Some(deadline)));
        match embed_res {
            Ok(outputs) => {
                let mut batch_records = Vec::with_capacity(outputs.len());
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
                        batch_records.push(EmbeddingRecord {
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
                        });
                    }
                }
                stats.embedded += batch_records.len() as u64;
                if let Some(gen_id) = target_gen_id {
                    store.put_batch_for_generation(&batch_records, gen_id)?;
                } else {
                    store.put_batch_and_mark_done(&batch_records)?;
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

        // [FIX] `.max(1)`: `validate()` already rejects a configured 0, but a
        // defensive floor here means a 0 that somehow slips through produces
        // a `BackgroundEnricher` that still does real work instead of one
        // that silently spawns no threads at all.
        let worker_count = cfg.embedding_worker_count.max(1);
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
                let conn = match Connection::open(&conn_path) {
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
    use crate::provider::{CancelFlag, EmbeddingFingerprint, EmbeddingOutput};

    #[allow(dead_code)]
    struct FingerprintedProvider(EmbeddingFingerprint);

    impl SemanticProvider for FingerprintedProvider {
        fn id(&self) -> &'static str {
            "fingerprinted-test"
        }
        fn model_id(&self) -> &str {
            "fingerprinted-test-v1"
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn max_input_bytes(&self) -> usize {
            4096
        }
        fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
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
}
