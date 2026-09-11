//! Semantic store (Phase 5 §8): a SEPARATE, disposable SQLite database.
//!
//! Deliberate design decisions (ADR-014):
//! * Lives in its own file (`semantic.db`) next to the canonical index —
//!   deleting it must never affect canonical intelligence (tested).
//! * Canonical SQLite entities are NOT contaminated with provider-specific
//!   vector assumptions; this file can be dropped and rebuilt at any time.
//! * Nearest-neighbor search uses a per-generation in-memory HNSW index
//!   (`hnsw_rs`, `DistCosine`) for O(log N) candidate retrieval at 30k–1M+
//!   vector scales, followed by exact full-dimension cosine reranking over
//!   SQLite B-tree lookups (ADR-015).  `semantic.db` remains the durable
//!   source of truth; the HNSW index is rebuilt incrementally on demand and
//!   is never persisted separately.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::error::SemanticError;
use crate::generation::{GenerationManager, GenerationRecord};
use crate::provider::{CancelFlag, EmbeddingFingerprint};
use hnsw_rs::prelude::*;
use rusqlite::{Connection, params};

const SEMANTIC_MIGRATION_0001: &str = include_str!("../../../migrations/semantic/0001_initial.sql");

/// One stored embedding with full lineage.
#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub retrieval_unit_id: String,
    pub repository_id: String,
    pub source_revision_id: String,
    pub index_generation_id: String,
    pub selection_version: String,
    pub provider_id: String,
    pub model_id: String,
    pub content_hash: String,
    pub dim: usize,
    pub vector: Vec<f32>,
}

/// One kNN hit.
#[derive(Debug, Clone, PartialEq)]
pub struct NearestHit {
    pub retrieval_unit_id: String,
    pub similarity: f32,
}

/// Enforceable scan bounds for nearest-neighbor search (§20). The scan
/// checks EVERY row against these bounds, so a large model can never turn a
/// bounded query into an unbounded wait.
#[derive(Debug)]
pub struct ScanBudget<'a> {
    /// Cooperative cancellation (query dropped / shutdown).
    pub cancel: &'a CancelFlag,
    /// Wall-clock deadline; `None` = no time bound.
    pub deadline: Option<std::time::Instant>,
    /// Hard cap on rows examined; `0` = unlimited.
    pub max_rows: u64,
}

impl<'a> ScanBudget<'a> {
    pub fn unbounded(cancel: &'a CancelFlag) -> Self {
        Self {
            cancel,
            deadline: None,
            max_rows: 0,
        }
    }

    fn exhausted(&self, scanned: u64) -> bool {
        if self.cancel.is_cancelled() {
            return true;
        }
        if let Some(d) = self.deadline
            && (scanned & 1023 == 0)
            && std::time::Instant::now() >= d
        {
            return true;
        }
        self.max_rows > 0 && scanned >= self.max_rows
    }
}

/// kNN outcome including honest observability about how much was searched.
#[derive(Debug, Clone)]
pub struct KnnResult {
    pub hits: Vec<NearestHit>,
    pub rows_scanned: u64,
    /// True when the scan stopped EARLY because of the budget (results are
    /// then best-effort, not exhaustive over the active model).
    pub truncated_by_budget: bool,
}

/// Queue row states.
pub const Q_PENDING: &str = "PENDING";
pub const Q_INFLIGHT: &str = "INFLIGHT";
pub const Q_DONE: &str = "DONE";
pub const Q_FAILED: &str = "FAILED";

/// A queued work item as returned by [`SemanticStore::queue_take_batch`].
#[derive(Debug, Clone)]
pub struct QueueItem {
    pub retrieval_unit_id: String,
    pub priority: f64,
    pub attempts: u32,
}

/// Minimal lineage metadata of a stored row (reconcile diff input).
#[derive(Debug, Clone)]
pub struct ActiveIdentityRow {
    pub retrieval_unit_id: String,
    pub content_hash: String,
    pub index_generation_id: String,
    pub selection_version: String,
}

/// Compact coarse vector dimension for stage-1 candidate search.
/// Qwen3 is an MRL (Matryoshka Representation Learning) model: the first 128 dimensions
/// capture the coarse semantic topology with high recall fidelity.
const COARSE_INDEX_DIM: usize = 128;

/// A lightweight candidate index entry cached in memory for high-throughput candidate search.
#[derive(Debug, Clone)]
struct CandidateEntry {
    rowid: i64,
    repository_id: String,
    deleted: bool,
}

/// Generation-isolated candidate index backed by HNSW for O(log N) fast retrieval.
///
/// Uses `DistCosine` rather than `DistDot` because L2-normalised embedding
/// vectors produced by neural models (Qwen3, etc.) contain negative
/// components.  `DistDot` in the underlying `anndists` crate asserts
/// `dot >= 0.0`, which panics on any such vector.  `DistCosine` has no sign
/// restriction and correctly orders nearest neighbours for signed float32
/// embeddings.
struct GenerationIndex {
    #[allow(dead_code)]
    generation_id: i64,
    hnsw: Hnsw<'static, f32, DistCosine>,
    metadata: HashMap<usize, CandidateEntry>,
    unit_to_ann_id: HashMap<String, usize>,
    last_synced_rowid: i64,
    next_ann_id: usize,
}

impl std::fmt::Debug for GenerationIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenerationIndex")
            .field("generation_id", &self.generation_id)
            .field("last_synced_rowid", &self.last_synced_rowid)
            .field("next_ann_id", &self.next_ann_id)
            .finish()
    }
}

impl GenerationIndex {
    pub fn new(generation_id: i64, capacity: usize) -> Self {
        Self {
            generation_id,
            // Configure HNSW parameters for dynamic capacity.
            // max_nb_connection: 16, max_elements: capacity, max_layer: 16, ef_construction: 200.
            // DistCosine is safe for signed float32 vectors; DistDot would panic on negative dot
            // products that arise naturally from L2-normalised neural embeddings.
            hnsw: Hnsw::new(16, capacity, 16, 200, DistCosine {}),
            metadata: HashMap::new(),
            unit_to_ann_id: HashMap::new(),
            last_synced_rowid: 0,
            next_ann_id: 1,
        }
    }

    pub fn tombstone_unit(&mut self, unit_id: &str) {
        if let Some(&ann_id) = self.unit_to_ann_id.get(unit_id) {
            if let Some(entry) = self.metadata.get_mut(&ann_id) {
                entry.deleted = true;
            }
        }
    }

    /// Stage 1: Candidate Search + Stage 2: Metadata Filtering
    /// Returns the top candidate rowids (up to candidate_limit) using scalable ANN search.
    fn search_candidates(
        &self,
        query: &[f32],
        candidate_limit: usize,
        repository_filter: Option<&str>,
        budget: &ScanBudget<'_>,
    ) -> (Vec<i64>, u64, bool) {
        if self.metadata.is_empty() || candidate_limit == 0 {
            return (Vec::new(), 0, false);
        }

        let mut q_coarse = [0.0f32; COARSE_INDEX_DIM];
        let copy_len = query.len().min(COARSE_INDEX_DIM);
        q_coarse[..copy_len].copy_from_slice(&query[..copy_len]);

        let q_norm: f32 = q_coarse.iter().map(|x| x * x).sum::<f32>().sqrt();
        if q_norm <= 0.0 {
            return (Vec::new(), 0, false);
        }
        let inv_qnorm = 1.0 / q_norm;
        // Normalize the query vector for DistDot (cosine similarity)
        for val in q_coarse.iter_mut() {
            *val *= inv_qnorm;
        }

        let mut truncated = false;
        let mut top_candidates = Vec::with_capacity(candidate_limit);

        // Start with a reasonable search budget and double it if filtering rejects too many
        let mut ef_search = candidate_limit.max(64);
        let max_ef_search = 10000;
        let mut searched_total: u64 = 0;

        loop {
            if budget.cancel.is_cancelled() {
                truncated = true;
                break;
            }
            if let Some(d) = budget.deadline
                && std::time::Instant::now() >= d
            {
                truncated = true;
                break;
            }

            let neighbors = self.hnsw.search(&q_coarse, ef_search, ef_search);
            top_candidates.clear();

            for neighbor in neighbors {
                if let Some(entry) = self.metadata.get(&neighbor.d_id) {
                    if entry.deleted {
                        continue;
                    }
                    if let Some(repo) = repository_filter
                        && entry.repository_id != repo
                    {
                        continue;
                    }
                    top_candidates.push(entry.rowid);
                    if top_candidates.len() >= candidate_limit {
                        break;
                    }
                }
            }

            searched_total += ef_search as u64;

            if top_candidates.len() >= candidate_limit || ef_search >= max_ef_search {
                break;
            }

            // Not enough candidates found after metadata filtering; increase search depth
            ef_search = (ef_search * 2).min(max_ef_search);
        }

        (top_candidates, searched_total, truncated)
    }
}

/// Shared-handle-safe semantic store: rusqlite connections are `!Sync`, so
/// every access goes through an internal mutex (contention is negligible at
/// Phase 5 scales; queries hold it only for bounded reads).
#[derive(Debug)]
pub struct SemanticStore {
    conn: Mutex<Connection>,
    candidate_index: Mutex<HashMap<i64, GenerationIndex>>,
}

impl SemanticStore {
    /// Open (creating if needed) the disposable semantic database.
    pub fn open(path: &Path) -> Result<Self, SemanticError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
        // Crash/power-loss resume semantics (§11): anything INFLIGHT when the
        // process died was never committed → reschedule it.
        conn.execute(
            "UPDATE sem_queue SET state = ?1 WHERE state = ?2",
            params![Q_PENDING, Q_INFLIGHT],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            candidate_index: Mutex::new(HashMap::new()),
        })
    }

    /// In-memory store for unit tests.
    pub fn open_in_memory() -> Result<Self, SemanticError> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            candidate_index: Mutex::new(HashMap::new()),
        })
    }

    /// Fallible lock acquisition: a poisoned mutex (a panic while the lock
    /// was held) must surface as [`SemanticError::StoreUnavailable`] and let
    /// callers degrade to canonical retrieval — NEVER an unwrap/panic.
    fn guard(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SemanticError> {
        self.conn
            .lock()
            .map_err(|_| SemanticError::StoreUnavailable("store mutex poisoned".into()))
    }

    /// TEST SUPPORT ONLY: deliberately poisons the internal mutex by panicking
    /// while holding the guard. Call from a sacrificial thread.
    #[doc(hidden)]
    pub fn debug_poison_mutex(&self) {
        let _g = self
            .conn
            .lock()
            .expect("poison helper requires healthy lock");
        panic!("intentional poison");
    }

    /// TEST/BENCHMARK SUPPORT ONLY: acquires the database guard directly.
    #[doc(hidden)]
    pub fn guard_for_test(&self) -> Result<std::sync::MutexGuard<'_, Connection>, SemanticError> {
        self.guard()
    }

    fn migrate(conn: &Connection) -> Result<(), SemanticError> {
        conn.execute_batch(SEMANTIC_MIGRATION_0001)?;
        Ok(())
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── learned tuning (Final Master Plan V2 §25, CP17) ───────────────────

    /// Read learned tuning matching the specified key.
    pub fn read_learned_tuning(
        &self,
        key: &crate::learned_tuning::TuningKey,
    ) -> Result<Option<crate::learned_tuning::LearnedTuningRecord>, SemanticError> {
        let conn = self.guard()?;
        crate::learned_tuning::LearnedTuningManager.read_tuning(&conn, key)
    }

    /// Save learned optimal execution tuning.
    pub fn save_learned_tuning(
        &self,
        record: &crate::learned_tuning::LearnedTuningRecord,
    ) -> Result<(), SemanticError> {
        let conn = self.guard()?;
        crate::learned_tuning::LearnedTuningManager.save_tuning(&conn, record)
    }

    /// Invalidate learned tuning for a specific key.
    pub fn invalidate_learned_tuning(
        &self,
        key: &crate::learned_tuning::TuningKey,
    ) -> Result<bool, SemanticError> {
        let conn = self.guard()?;
        crate::learned_tuning::LearnedTuningManager.invalidate_tuning(&conn, key)
    }

    // ── embeddings ─────────────────────────────────────────────────────────

    /// Insert or replace one embedding (idempotent per unit+model).
    pub fn put(&self, rec: &EmbeddingRecord) -> Result<(), SemanticError> {
        let norm: f32 = rec.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        let mut blob = Vec::with_capacity(rec.vector.len() * 4);
        for v in &rec.vector {
            blob.extend_from_slice(&v.to_le_bytes());
        }
        self.guard()?.execute(
            "INSERT OR REPLACE INTO sem_embeddings
                 (retrieval_unit_id, repository_id, source_revision_id,
                  index_generation_id, selection_version, provider_id, model_id,
                  content_hash, dim, norm, vector, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                rec.retrieval_unit_id,
                rec.repository_id,
                rec.source_revision_id,
                rec.index_generation_id,
                rec.selection_version,
                rec.provider_id,
                rec.model_id,
                rec.content_hash,
                rec.dim as i64,
                norm,
                blob,
                Self::now_ms()
            ],
        )?;
        Ok(())
    }

    /// Atomically persists a batch of embeddings and marks their queue entries
    /// DONE under a single mutex acquisition and one SQLite transaction — the
    /// batched counterpart to calling `put()` + `queue_mark_done()` once per
    /// record, which issues 2 auto-committed statements per unit.
    pub fn put_batch_and_mark_done(
        &self,
        records: &[EmbeddingRecord],
    ) -> Result<(), SemanticError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut conn = self.guard()?;
        let tx = conn.transaction()?;

        {
            let mut insert_stmt = tx.prepare(
                "INSERT OR REPLACE INTO sem_embeddings
                     (retrieval_unit_id, repository_id, source_revision_id,
                      index_generation_id, selection_version, provider_id, model_id,
                      content_hash, dim, norm, vector, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            let mut mark_stmt =
                tx.prepare("UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1")?;

            let now = Self::now_ms();
            for rec in records {
                let norm: f32 = rec.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                let mut blob = Vec::with_capacity(rec.vector.len() * 4);
                for v in &rec.vector {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
                insert_stmt.execute(params![
                    rec.retrieval_unit_id,
                    rec.repository_id,
                    rec.source_revision_id,
                    rec.index_generation_id,
                    rec.selection_version,
                    rec.provider_id,
                    rec.model_id,
                    rec.content_hash,
                    rec.dim as i64,
                    norm,
                    blob,
                    now,
                ])?;
                mark_stmt.execute(params![rec.retrieval_unit_id, Q_DONE])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    fn tombstone_units_in_all_indexes<'a>(&self, unit_ids: impl Iterator<Item = &'a str>) {
        let ids: Vec<&str> = unit_ids.collect();
        if ids.is_empty() {
            return;
        }
        if let Ok(mut guard) = self.candidate_index.lock() {
            for gen_idx in guard.values_mut() {
                for unit_id in &ids {
                    gen_idx.tombstone_unit(unit_id);
                }
            }
        }
    }

    /// Delete every embedding for one unit (all models) or one exact record
    /// when `provider`/`model` are given.
    pub fn delete(
        &self,
        unit_id: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<usize, SemanticError> {
        let n = match (provider, model) {
            (Some(p), Some(m)) => self.guard()?.execute(
                "DELETE FROM sem_embeddings
                      WHERE retrieval_unit_id=?1 AND provider_id=?2 AND model_id=?3",
                params![unit_id, p, m],
            )?,
            _ => self.guard()?.execute(
                "DELETE FROM sem_embeddings WHERE retrieval_unit_id=?1",
                params![unit_id],
            )?,
        };
        if n > 0 {
            self.tombstone_units_in_all_indexes(std::iter::once(unit_id));
        }
        Ok(n)
    }

    /// Lookup by exact semantic-unit identity components + model.
    pub fn lookup(
        &self,
        unit_id: &str,
        provider: &str,
        model: &str,
    ) -> Result<Option<EmbeddingRecord>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT retrieval_unit_id, repository_id, source_revision_id,
                    index_generation_id, selection_version, provider_id, model_id,
                    content_hash, dim, vector
               FROM sem_embeddings
              WHERE retrieval_unit_id=?1 AND provider_id=?2 AND model_id=?3",
        )?;
        let mut rows = stmt.query(params![unit_id, provider, model])?;
        if let Some(r) = rows.next()? {
            Ok(Some(Self::row_to_record(r)?))
        } else {
            Ok(None)
        }
    }

    fn row_to_record(r: &rusqlite::Row<'_>) -> Result<EmbeddingRecord, SemanticError> {
        let dim_i64: i64 = r.get(8)?;
        let dim = dim_i64.max(0) as usize;
        let blob: Vec<u8> = r.get(9)?;
        let mut vec = Vec::with_capacity(dim);
        let floats = blob.as_chunks::<4>().0;
        for chunk in floats {
            vec.push(f32::from_le_bytes(*chunk));
        }
        Ok(EmbeddingRecord {
            retrieval_unit_id: r.get(0)?,
            repository_id: r.get(1)?,
            source_revision_id: r.get(2)?,
            index_generation_id: r.get(3)?,
            selection_version: r.get(4)?,
            provider_id: r.get(5)?,
            model_id: r.get(6)?,
            content_hash: r.get(7)?,
            dim,
            vector: vec,
        })
    }

    /// Delete ALL embeddings whose (provider, model) differ from the active
    /// pair — model-change invalidation without touching canonical data.
    pub fn purge_inactive_models(
        &self,
        active_provider: &str,
        active_model: &str,
    ) -> Result<usize, SemanticError> {
        let n = self.guard()?.execute(
            "DELETE FROM sem_embeddings WHERE provider_id!=?1 OR model_id!=?2",
            params![active_provider, active_model],
        )?;
        if n > 0 {
            self.candidate_index.lock().unwrap().clear();
        }
        Ok(n)
    }

    /// Delete everything for one model (full semantic-layer reset).
    pub fn purge_model(&self, provider: &str, model: &str) -> Result<usize, SemanticError> {
        let n = self.guard()?.execute(
            "DELETE FROM sem_embeddings WHERE provider_id=?1 AND model_id=?2",
            params![provider, model],
        )?;
        if n > 0 {
            self.candidate_index.lock().unwrap().clear();
        }
        Ok(n)
    }

    /// Count embeddings for a (provider, model), optionally per repository.
    pub fn count(
        &self,
        provider: &str,
        model: &str,
        repository_filter: Option<&str>,
    ) -> Result<u64, SemanticError> {
        Ok(self.guard()?.query_row(
            "SELECT COUNT(*) FROM sem_embeddings
              WHERE provider_id=?1 AND model_id=?2
                AND (?3 IS NULL OR repository_id=?3)",
            params![provider, model, repository_filter],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    /// Distinct (provider, model) pairs present with row counts.
    pub fn model_inventory(&self) -> Result<HashMap<(String, String), u64>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT provider_id, model_id, COUNT(*) FROM sem_embeddings
             GROUP BY provider_id, model_id",
        )?;
        let mut out = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            out.insert(
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, i64>(2)? as u64,
            );
        }
        Ok(out)
    }

    // ── semantic generations (Phase V2 §51–§55) ───────────────────────────

    /// Retrieve the currently ACTIVE generation, if any.
    pub fn get_active_generation(&self) -> Result<Option<GenerationRecord>, SemanticError> {
        let conn = self.guard()?;
        GenerationManager::get_active_generation(&conn)
    }

    /// Retrieve the currently BUILDING generation, if any.
    pub fn get_building_generation(&self) -> Result<Option<GenerationRecord>, SemanticError> {
        let conn = self.guard()?;
        GenerationManager::get_building_generation(&conn)
    }

    /// Start a new semantic generation in BUILDING state.
    pub fn start_new_generation(
        &self,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<GenerationRecord, SemanticError> {
        let conn = self.guard()?;
        GenerationManager::start_new_generation(&conn, fingerprint)
    }

    /// Atomically activate a generation, superseding the previously active generation.
    pub fn activate_generation(&self, target_generation_id: i64) -> Result<(), SemanticError> {
        let mut conn = self.guard()?;
        GenerationManager::activate_generation(&mut conn, target_generation_id)
    }

    /// Roll back to the most recent superseded complete generation.
    pub fn rollback_generation(&self) -> Result<Option<GenerationRecord>, SemanticError> {
        let mut conn = self.guard()?;
        GenerationManager::rollback_to_previous(&mut conn)
    }

    /// Prune old superseded generations according to retention policy.
    ///
    /// In addition to deleting DB rows via [`GenerationManager::prune_old_generations`],
    /// this also evicts the corresponding in-memory HNSW entries from
    /// `candidate_index` so the memory they hold (HNSW graph + metadata map)
    /// is promptly released.  The eviction query runs while `conn` is still
    /// held so that the surviving-ID set is consistent with the post-prune DB
    /// state; `conn` is then dropped before acquiring `candidate_index` to
    /// preserve the established lock ordering (conn → candidate_index).
    pub fn prune_generations(&self, keep_max: usize) -> Result<usize, SemanticError> {
        let mut conn = self.guard()?;
        let pruned = GenerationManager::prune_old_generations(&mut conn, keep_max)?;

        if pruned > 0 {
            // Collect the generation IDs that still exist post-prune.
            let surviving: std::collections::HashSet<i64> = {
                let mut stmt = conn.prepare("SELECT generation_id FROM sem_generations")?;
                let mut rows = stmt.query([])?;
                let mut set = std::collections::HashSet::new();
                while let Some(r) = rows.next()? {
                    set.insert(r.get::<_, i64>(0)?);
                }
                set
            };
            // Release the DB lock before acquiring candidate_index to maintain
            // the consistent conn → candidate_index lock order.
            drop(conn);

            let mut index_guard = self.candidate_index.lock().map_err(|_| {
                SemanticError::StoreUnavailable("candidate index mutex poisoned".into())
            })?;
            index_guard.retain(|gen_id, _| surviving.contains(gen_id));
        }

        Ok(pruned)
    }

    /// Insert a batch of embedding records tagged with a specific generation ID (§52).
    pub fn put_batch_for_generation(
        &self,
        records: &[EmbeddingRecord],
        generation_id: i64,
    ) -> Result<(), SemanticError> {
        if records.is_empty() {
            return Ok(());
        }

        let mut conn = self.guard()?;
        let tx = conn.transaction()?;

        {
            let mut insert_stmt = tx.prepare(
                "INSERT OR REPLACE INTO sem_embeddings
                     (retrieval_unit_id, repository_id, source_revision_id,
                      index_generation_id, selection_version, provider_id, model_id,
                      content_hash, dim, norm, vector, created_at_ms, generation_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            let mut mark_stmt =
                tx.prepare("UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1")?;
            let mut check_stmt = tx.prepare(
                "SELECT 1 FROM sem_embeddings WHERE retrieval_unit_id=?1 AND provider_id=?2 AND model_id=?3 AND generation_id=?4"
            )?;

            let mut new_units = 0i64;
            let now = Self::now_ms();
            for rec in records {
                if !check_stmt.exists(params![
                    rec.retrieval_unit_id,
                    rec.provider_id,
                    rec.model_id,
                    generation_id
                ])? {
                    new_units += 1;
                }

                let norm: f32 = rec.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                let mut blob = Vec::with_capacity(rec.vector.len() * 4);
                for v in &rec.vector {
                    blob.extend_from_slice(&v.to_le_bytes());
                }
                insert_stmt.execute(params![
                    rec.retrieval_unit_id,
                    rec.repository_id,
                    rec.source_revision_id,
                    rec.index_generation_id,
                    rec.selection_version,
                    rec.provider_id,
                    rec.model_id,
                    rec.content_hash,
                    rec.dim as i64,
                    norm,
                    blob,
                    now,
                    generation_id,
                ])?;
                mark_stmt.execute(params![rec.retrieval_unit_id, Q_DONE])?;
            }

            if new_units > 0 {
                tx.execute(
                    "UPDATE sem_generations SET unit_count = unit_count + ?1 WHERE generation_id = ?2",
                    params![new_units, generation_id],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Synchronize the in-memory generation candidate index from SQLite for the given generation ID.
    pub fn ensure_candidate_index_synced(
        &self,
        conn: &Connection,
        generation_id: i64,
    ) -> Result<(), SemanticError> {
        let mut index_guard = self.candidate_index.lock().map_err(|_| {
            SemanticError::StoreUnavailable("candidate index mutex poisoned".into())
        })?;

        // Fast path check if generation exists and its count
        let unit_count = conn
            .query_row(
                "SELECT unit_count FROM sem_generations WHERE generation_id = ?1",
                params![generation_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            .max(0) as usize;
        let capacity = unit_count.max(10_000) + 1_000_000;

        let gen_idx = index_guard
            .entry(generation_id)
            .or_insert_with(|| GenerationIndex::new(generation_id, capacity));

        let mut stmt = conn.prepare(
            "SELECT rowid, retrieval_unit_id, repository_id, norm, substr(vector, 1, 512)
             FROM sem_embeddings
             WHERE generation_id = ?1 AND rowid > ?2
             ORDER BY rowid ASC",
        )?;
        let mut rows = stmt.query(params![generation_id, gen_idx.last_synced_rowid])?;
        while let Some(r) = rows.next()? {
            let rowid: i64 = r.get(0)?;
            let unit_id: String = r.get(1)?;
            let repo_id: String = r.get(2)?;
            let _norm: f32 = r.get(3)?;
            let blob: Vec<u8> = r.get(4)?;

            let mut coarse = [0.0f32; COARSE_INDEX_DIM];
            let floats_to_read = (blob.len() / 4).min(COARSE_INDEX_DIM);
            for i in 0..floats_to_read {
                coarse[i] = f32::from_le_bytes([
                    blob[i * 4],
                    blob[i * 4 + 1],
                    blob[i * 4 + 2],
                    blob[i * 4 + 3],
                ]);
            }
            let coarse_norm = coarse.iter().map(|x| x * x).sum::<f32>().sqrt();
            if coarse_norm > 0.0 {
                let inv_norm = 1.0 / coarse_norm;
                for val in coarse.iter_mut() {
                    *val *= inv_norm;
                }
            }

            let ann_id = gen_idx.next_ann_id;
            gen_idx.next_ann_id += 1;

            if let Some(old_ann_id) = gen_idx.unit_to_ann_id.insert(unit_id, ann_id) {
                if let Some(entry) = gen_idx.metadata.get_mut(&old_ann_id) {
                    entry.deleted = true;
                }
            }

            gen_idx.hnsw.insert((&coarse, ann_id));
            gen_idx.metadata.insert(
                ann_id,
                CandidateEntry {
                    rowid,
                    repository_id: repo_id,
                    deleted: false,
                },
            );
            gen_idx.last_synced_rowid = rowid;
        }
        Ok(())
    }

    /// Stage 3: Exact Cosine Rerank.
    /// Fetches the full high-precision vector blobs for candidate rowids from SQLite via direct B-tree lookup,
    /// computes exact cosine similarities across the full dimension (e.g. 512 or 1024),
    /// and returns the top k nearest hits.
    fn rerank_candidates(
        conn: &Connection,
        query: &[f32],
        qnorm: f32,
        k: usize,
        candidate_rowids: &[i64],
    ) -> Result<Vec<NearestHit>, SemanticError> {
        if candidate_rowids.is_empty() || k == 0 {
            return Ok(Vec::new());
        }

        let mut top: Vec<(f32, String)> = Vec::with_capacity(k + 1);

        for chunk in candidate_rowids.chunks(100) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT retrieval_unit_id, norm, vector FROM sem_embeddings
                 WHERE rowid IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len());
            for id in chunk {
                params_vec.push(id);
            }

            let mut rows = stmt.query(params_vec.as_slice())?;
            while let Some(r) = rows.next()? {
                let unit_id: String = r.get(0)?;
                let stored_norm: f32 = r.get(1)?;
                let blob: Vec<u8> = r.get(2)?;

                if stored_norm <= 0.0 || blob.len() != query.len() * 4 {
                    continue;
                }

                let mut dot = 0.0f32;
                let floats = blob.as_chunks::<4>().0;
                for (i, chunk) in floats.iter().enumerate() {
                    let b = f32::from_le_bytes(*chunk);
                    dot += query[i] * b;
                }
                let sim = dot / (qnorm * stored_norm);

                if top.len() == k && sim <= top.last().map_or(f32::MIN, |(s, _)| *s) {
                    continue;
                }
                let pos = top.partition_point(|(s, _)| *s >= sim);
                top.insert(pos, (sim, unit_id));
                if top.len() > k {
                    top.pop();
                }
            }
        }

        let hits = top
            .into_iter()
            .map(|(sim, id)| NearestHit {
                retrieval_unit_id: id,
                similarity: sim,
            })
            .collect();
        Ok(hits)
    }

    /// Search nearest neighbors strictly isolated within a specific semantic generation (§52).
    /// Employs real production two-stage retrieval:
    ///   1. Candidate Search: fast coarse vector index scan over generation vectors.
    ///   2. Metadata Filtering: enforces repository scoping and generation isolation.
    ///   3. Exact Cosine Rerank: loads exact full-dimension vectors for candidates and reranks.
    pub fn knn_search_generation(
        &self,
        generation_id: i64,
        query: &[f32],
        k: usize,
        repository_filter: Option<&str>,
        budget: &ScanBudget<'_>,
    ) -> Result<KnnResult, SemanticError> {
        let qnorm: f32 = query.iter().map(|x| x * x).sum::<f32>().sqrt();
        if qnorm <= 0.0 || k == 0 || budget.exhausted(0) {
            return Ok(KnnResult {
                hits: Vec::new(),
                rows_scanned: 0,
                truncated_by_budget: budget.max_rows > 0 || budget.deadline.is_some(),
            });
        }

        let conn = self.guard()?;

        // Ensure generation candidate index is up-to-date
        self.ensure_candidate_index_synced(&conn, generation_id)?;

        // Candidate limit: top candidates to advance to exact reranking
        let candidate_limit = (k * 20).max(100);

        // Stage 1 & 2: Candidate search with metadata filtering
        let (candidate_ids, rows_scanned, truncated) = {
            let index_guard = self.candidate_index.lock().unwrap();
            let gen_idx = index_guard
                .get(&generation_id)
                .expect("index synced immediately above");
            gen_idx.search_candidates(query, candidate_limit, repository_filter, budget)
        };

        if candidate_ids.is_empty() {
            return Ok(KnnResult {
                hits: Vec::new(),
                rows_scanned,
                truncated_by_budget: truncated,
            });
        }

        // Stage 3: Exact Cosine Rerank
        let hits = Self::rerank_candidates(&conn, query, qnorm, k, &candidate_ids)?;

        Ok(KnnResult {
            hits,
            rows_scanned,
            truncated_by_budget: truncated,
        })
    }

    // ── enrichment queue ───────────────────────────────────────────────────

    /// Enqueue units that are not DONE already. Priority replaces existing
    /// entries (demand-driven re-prioritization is inspectable state).
    pub fn queue_enqueue(&self, unit_ids: &[String], priority: f64) -> Result<(), SemanticError> {
        let t = Self::now_ms();
        for id in unit_ids {
            self.guard()?.execute(
                "INSERT INTO sem_queue (retrieval_unit_id, priority, state, attempts, enqueued_at_ms)
                 VALUES (?1, ?2, ?3, 0, ?4)
                 ON CONFLICT(retrieval_unit_id) DO UPDATE
                   SET priority=excluded.priority, state=?3, enqueued_at_ms=?4",
                params![id, priority, Q_PENDING, t],
            )?;
        }
        Ok(())
    }

    /// Scored variant: per-unit priority from selection scores.
    pub fn queue_enqueue_scored(&self, items: &[(String, f64)]) -> Result<(), SemanticError> {
        let t = Self::now_ms();
        for (id, priority) in items {
            self.guard()?.execute(
                "INSERT INTO sem_queue (retrieval_unit_id, priority, state, attempts, enqueued_at_ms)
                 VALUES (?1, ?2, ?3, 0, ?4)
                 ON CONFLICT(retrieval_unit_id) DO UPDATE
                   SET priority=excluded.priority, state=?3, enqueued_at_ms=?4",
                params![id, priority, Q_PENDING, t],
            )?;
        }
        Ok(())
    }

    /// Atomically claim up to `limit` PENDING items (priority DESC, FIFO
    /// within equal priority) and mark them INFLIGHT in one statement.
    ///
    /// [FIX] Previously a two-phase SELECT-then-UPDATE-loop: the guard was
    /// released between the read and the write, so two threads racing this
    /// call could both SELECT the same PENDING rows before either UPDATE
    /// landed, double-claiming the same work. A single `UPDATE ... RETURNING`
    /// statement, executed while holding one `guard()` acquisition for the
    /// whole call, makes the claim atomic — SQLite's own serialization of
    /// writers means no other connection can observe or claim these rows
    /// between the SELECT-subquery and the UPDATE.
    pub fn queue_take_batch(&self, limit: usize) -> Result<Vec<QueueItem>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "UPDATE sem_queue SET state = ?1
              WHERE retrieval_unit_id IN (
                  SELECT retrieval_unit_id FROM sem_queue
                  WHERE state = ?2
                  ORDER BY priority DESC, enqueued_at_ms ASC, retrieval_unit_id ASC
                  LIMIT ?3
              )
              RETURNING retrieval_unit_id, priority, attempts",
        )?;
        let mut out = Vec::new();
        let mut rows = stmt.query(params![Q_INFLIGHT, Q_PENDING, limit as i64])?;
        while let Some(r) = rows.next()? {
            out.push(QueueItem {
                retrieval_unit_id: r.get(0)?,
                priority: r.get(1)?,
                attempts: r.get::<_, i64>(2)? as u32,
            });
        }
        Ok(out)
    }

    pub fn queue_mark_done(&self, unit_id: &str) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1",
            params![unit_id, Q_DONE],
        )?;
        Ok(())
    }

    pub fn queue_mark_failed(&self, unit_id: &str, max_attempts: u32) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "UPDATE sem_queue
                SET attempts = attempts + 1,
                    state = CASE WHEN attempts + 1 >= ?2 THEN ?3 ELSE ?4 END
              WHERE retrieval_unit_id=?1",
            params![unit_id, max_attempts as i64, Q_FAILED, Q_PENDING],
        )?;
        Ok(())
    }

    /// Permanently quarantine an item (security refusal / hard-invalid).
    pub fn queue_fail_permanently(&self, unit_id: &str) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1",
            params![unit_id, Q_FAILED],
        )?;
        Ok(())
    }

    /// Return an INFLIGHT item to PENDING (cancellation / crash resume).
    /// Items already DONE are unaffected.
    pub fn queue_reset(&self, unit_id: &str) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1 AND state=?3",
            params![unit_id, Q_PENDING, Q_INFLIGHT],
        )?;
        Ok(())
    }

    /// Minimal identity metadata for every stored row of the ACTIVE model —
    /// the reconcile diff input.
    pub fn active_identity_rows(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<Vec<ActiveIdentityRow>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT retrieval_unit_id, content_hash, index_generation_id,
                    selection_version
               FROM sem_embeddings WHERE provider_id=?1 AND model_id=?2",
        )?;
        let mut out = Vec::new();
        let mut rows = stmt.query(params![provider, model])?;
        while let Some(r) = rows.next()? {
            out.push(ActiveIdentityRow {
                retrieval_unit_id: r.get(0)?,
                content_hash: r.get(1)?,
                index_generation_id: r.get(2)?,
                selection_version: r.get(3)?,
            });
        }
        Ok(out)
    }

    /// Drop queue entries for units no longer selected/existing.
    ///
    /// [FIX] Guarded with `AND state != Q_INFLIGHT` (both branches) so this
    /// never deletes a row another thread currently owns mid-embedding —
    /// without this, a concurrent `drive()` claim (INFLIGHT via
    /// `queue_take_batch`) racing a `reconcile()` call here could have its
    /// row deleted out from under it; the later `queue_mark_done`/
    /// `queue_mark_failed`/`queue_reset` would then silently affect zero
    /// rows instead of the claimed item.
    pub fn queue_retain_only(&self, keep: &[String]) -> Result<usize, SemanticError> {
        use rusqlite::ToSql;
        let n = if keep.is_empty() {
            self.conn.lock().expect("semantic store mutex").execute(
                "DELETE FROM sem_queue WHERE state != ?1",
                params![Q_INFLIGHT],
            )?
        } else {
            let mut paramslice: Vec<&dyn ToSql> = keep.iter().map(|s| s as &dyn ToSql).collect();
            paramslice.push(&Q_INFLIGHT as &dyn ToSql);
            let placeholders = vec!["?"; keep.len()].join(",");
            let sql = format!(
                "DELETE FROM sem_queue WHERE retrieval_unit_id NOT IN ({placeholders}) AND state != ?"
            );
            self.conn
                .lock()
                .expect("semantic store mutex")
                .execute(&sql, paramslice.as_slice())?
        };
        Ok(n)
    }

    pub fn queue_counts(&self) -> Result<HashMap<String, u64>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare("SELECT state, COUNT(*) FROM sem_queue GROUP BY state")?;
        let mut out = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            out.insert(r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64);
        }
        Ok(out)
    }

    /// Remove DONE rows entirely (bounded queue; done work needs no history).
    pub fn queue_prune_done(&self) -> Result<usize, SemanticError> {
        let conn = self.guard()?;
        Ok(conn.execute("DELETE FROM sem_queue WHERE state=?1", params![Q_DONE])?)
    }

    // ── query demand (§4 signal; disposable observability) ─────────────────

    pub fn bump_demand(&self, paths: &[String]) -> Result<(), SemanticError> {
        let t = Self::now_ms();
        for p in paths {
            self.guard()?.execute(
                "INSERT INTO sem_query_demand (path, hits, last_at_ms) VALUES (?1, 1, ?2)
                 ON CONFLICT(path) DO UPDATE SET hits = hits + 1, last_at_ms = ?2",
                params![p, t],
            )?;
        }
        Ok(())
    }

    pub fn demand_map(&self) -> Result<HashMap<String, u64>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare("SELECT path, hits FROM sem_query_demand")?;
        let mut out = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            out.insert(r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64);
        }
        Ok(out)
    }

    /// Approximate on-disk size of the semantic layer (observability §21/§23).
    pub fn file_size_bytes(path: &Path) -> u64 {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    }

    // ── maintenance ──────────────────────────────────────────────────────

    /// Run `semantic.db` production maintenance: an explicit WAL TRUNCATE
    /// checkpoint plus an optional `VACUUM`, mirroring
    /// `attic_storage::connection::run_maintenance` for the canonical
    /// database. Like that counterpart, this is intended to be called
    /// periodically or at clean shutdown — never from inside a held
    /// [`SemanticStore::guard`] elsewhere in the same call stack.
    ///
    /// `semantic.db` accumulates free pages from `delete`,
    /// `purge_inactive_models`, `purge_model`, and `queue_prune_done`;
    /// without an occasional `vacuum: true` call the file never shrinks.
    ///
    /// `VACUUM` must NOT run while a transaction is open on the underlying
    /// connection. This function checks `Connection::is_autocommit()` first
    /// and fails closed with [`SemanticError::StoreUnavailable`] instead of
    /// letting SQLite raise its own "cannot VACUUM from within a
    /// transaction" error.
    pub fn run_maintenance(&self, vacuum: bool) -> Result<(), SemanticError> {
        let conn = self.guard()?;
        // Explicit TRUNCATE checkpoint: flush WAL content into the main file
        // and truncate the WAL to zero length.
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_row| Ok(()))?;
        if vacuum {
            if !conn.is_autocommit() {
                return Err(SemanticError::StoreUnavailable(
                    "run_maintenance: VACUUM requested while a transaction is open on the semantic store connection"
                        .to_string(),
                ));
            }
            conn.execute_batch("VACUUM")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_schema_is_migration_owned_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        SemanticStore::migrate(&conn).unwrap();
        SemanticStore::migrate(&conn).unwrap();

        let applied: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sem_schema_migrations WHERE id='0001_initial'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied, 1);

        for table in &["sem_embeddings", "sem_queue", "sem_query_demand"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [*table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "semantic table {table} must exist");
        }
    }

    fn rec(unit: &str, vec: Vec<f32>) -> EmbeddingRecord {
        EmbeddingRecord {
            retrieval_unit_id: unit.to_owned(),
            repository_id: "repo-a".into(),
            source_revision_id: "rev-1".into(),
            index_generation_id: "gen-1".into(),
            selection_version: "v1".into(),
            provider_id: "hashing".into(),
            model_id: "hashed-ngram-v1".into(),
            content_hash: crate::identity::content_hash(unit),
            dim: vec.len(),
            vector: vec,
        }
    }

    #[test]
    fn put_lookup_delete_roundtrip() {
        let s = SemanticStore::open_in_memory().unwrap();
        s.put(&rec("u1", vec![1.0, 0.0])).unwrap();
        let got = s
            .lookup("u1", "hashing", "hashed-ngram-v1")
            .unwrap()
            .unwrap();
        assert_eq!(got.vector, vec![1.0, 0.0]);
        assert_eq!(s.delete("u1", None, None).unwrap(), 1);
        assert!(
            s.lookup("u1", "hashing", "hashed-ngram-v1")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn put_batch_and_mark_done_persists_records_and_updates_queue() {
        let s = SemanticStore::open_in_memory().unwrap();
        s.queue_enqueue(&["u1".into(), "u2".into()], 1.0).unwrap();
        let items = s.queue_take_batch(2).unwrap();
        assert_eq!(items.len(), 2);

        let records = vec![rec("u1", vec![1.0, 0.0]), rec("u2", vec![0.0, 1.0])];
        s.put_batch_and_mark_done(&records).unwrap();

        let got1 = s
            .lookup("u1", "hashing", "hashed-ngram-v1")
            .unwrap()
            .unwrap();
        assert_eq!(got1.vector, vec![1.0, 0.0]);
        let got2 = s
            .lookup("u2", "hashing", "hashed-ngram-v1")
            .unwrap()
            .unwrap();
        assert_eq!(got2.vector, vec![0.0, 1.0]);

        let counts = s.queue_counts().unwrap();
        assert_eq!(counts.get(Q_DONE), Some(&2));
        assert_eq!(counts.get(Q_INFLIGHT).copied().unwrap_or(0), 0);
        assert_eq!(counts.get(Q_PENDING).copied().unwrap_or(0), 0);
    }

    #[test]
    fn purge_inactive_models_keeps_active_pair() {
        let s = SemanticStore::open_in_memory().unwrap();
        s.put(&rec("keep", vec![1.0])).unwrap();
        let mut old = rec("drop", vec![0.5]);
        old.model_id = "ancient".into();
        s.put(&old).unwrap();
        let removed = s
            .purge_inactive_models("hashing", "hashed-ngram-v1")
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.count("hashing", "hashed-ngram-v1", None).unwrap(), 1);
    }

    #[test]
    fn queue_lifecycle_and_crash_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        {
            let s = SemanticStore::open(&path).unwrap();
            s.queue_enqueue(&["a".into(), "b".into()], 0.9).unwrap();
            let batch = s.queue_take_batch(1).unwrap();
            assert_eq!(batch[0].retrieval_unit_id, "a");
            // crash before marking done: "a" stays INFLIGHT on disk
        }
        {
            let s = SemanticStore::open(&path).unwrap(); // reopen after "crash"
            let counts = s.queue_counts().unwrap();
            assert_eq!(
                counts.get(Q_INFLIGHT).copied().unwrap_or(0),
                0,
                "inflight work must be gone after recovery"
            );
            assert_eq!(counts.get(Q_PENDING), Some(&2), "inflight rescheduled");
            s.queue_mark_done("a").unwrap();
            s.queue_mark_failed("b", 3).unwrap();
            s.queue_mark_failed("b", 3).unwrap();
            s.queue_mark_failed("b", 3).unwrap();
            let counts = s.queue_counts().unwrap();
            assert_eq!(counts.get(Q_FAILED), Some(&1));
        }
    }

    // -----------------------------------------------------------------------
    // Maintenance (Bug 14: semantic.db had zero VACUUM capability)
    // -----------------------------------------------------------------------

    #[test]
    fn run_maintenance_without_vacuum_succeeds_on_fresh_store() {
        let s = SemanticStore::open_in_memory().unwrap();
        s.run_maintenance(false).unwrap();
    }

    #[test]
    fn run_maintenance_with_vacuum_succeeds_and_does_not_grow_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        let s = SemanticStore::open(&path).unwrap();

        // Generate some churn so VACUUM has real (if modest) free space to
        // reclaim, then check the on-disk size sanity: it must not GROW as a
        // result of running maintenance.
        for i in 0..20 {
            s.put(&rec(&format!("u{i}"), vec![1.0, 0.0])).unwrap();
        }
        for i in 0..20 {
            s.delete(&format!("u{i}"), None, None).unwrap();
        }

        // Checkpoint first (no vacuum) so the "before" size reflects the main
        // db file with all WAL content already flushed in — otherwise, in
        // WAL mode, most of this data still lives in the `-wal` file and the
        // main file looks artificially tiny, making the very act of
        // checkpointing (which `run_maintenance` also does) look like
        // "growth" caused by VACUUM.
        s.run_maintenance(false).unwrap();
        let size_before = SemanticStore::file_size_bytes(&path);
        s.run_maintenance(true)
            .expect("maintenance with vacuum must succeed against a fresh semantic db");
        let size_after = SemanticStore::file_size_bytes(&path);
        assert!(
            size_after <= size_before,
            "VACUUM must not grow the file: before={size_before} after={size_after}"
        );
    }

    #[test]
    fn run_maintenance_vacuum_rejected_inside_open_transaction() {
        let s = SemanticStore::open_in_memory().unwrap();
        {
            // Hold the guard and open a transaction directly on the
            // underlying connection to simulate the unsafe precondition;
            // `run_maintenance` takes its own guard, so this must be dropped
            // before calling it (the mutex is not reentrant).
            let conn = s.guard().unwrap();
            conn.execute_batch("BEGIN").unwrap();
        }
        // NOTE: the transaction above is scoped to the guard's lifetime only
        // in the sense of when we release the lock; the underlying SQLite
        // connection itself remains mid-transaction until COMMIT/ROLLBACK.
        let result = s.run_maintenance(true);
        assert!(
            matches!(result, Err(SemanticError::StoreUnavailable(_))),
            "expected StoreUnavailable rejecting VACUUM mid-transaction, got {result:?}"
        );
        // Clean up: end the transaction so the connection can be dropped
        // cleanly.
        s.guard().unwrap().execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn semantic_generations_isolation_and_rollback_lifecycle() {
        let store = SemanticStore::open_in_memory().unwrap();
        let cancel = CancelFlag::new();
        let budget = ScanBudget::unbounded(&cancel);

        // Gen 1: Qwen3 Revision 1
        let fp1 = EmbeddingFingerprint {
            provider: "qwen3".to_string(),
            model_id: "qwen3-embedding-0.6b".to_string(),
            model_revision: "rev1".to_string(),
            dimension: 2,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "tok_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_retrieval_v1".to_string(),
        };
        let gen1 = store.start_new_generation(&fp1).unwrap();
        assert_eq!(gen1.generation_id, 1);
        store.activate_generation(gen1.generation_id).unwrap();

        // Insert unit A in Gen 1
        let rec_a = EmbeddingRecord {
            retrieval_unit_id: "unit_a".to_string(),
            repository_id: "repo1".to_string(),
            source_revision_id: "s1".to_string(),
            index_generation_id: "i1".to_string(),
            selection_version: "v1".to_string(),
            provider_id: "qwen3".to_string(),
            model_id: "qwen3-embedding-0.6b".to_string(),
            content_hash: "hash_a".to_string(),
            dim: 2,
            vector: vec![1.0, 0.0],
        };
        store.put_batch_for_generation(&[rec_a], 1).unwrap();

        // Query Gen 1
        let res1 = store
            .knn_search_generation(1, &[1.0, 0.0], 5, None, &budget)
            .unwrap();
        assert_eq!(res1.hits.len(), 1);
        assert_eq!(res1.hits[0].retrieval_unit_id, "unit_a");

        // Start Gen 2: Qwen3
        let fp2 = EmbeddingFingerprint {
            provider: "qwen3".to_string(),
            model_id: "qwen3-0.6b".to_string(),
            model_revision: "rev2".to_string(),
            dimension: 2,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "qwen_tok".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_retrieval_v1".to_string(),
        };
        let gen2 = store.start_new_generation(&fp2).unwrap();
        assert_eq!(gen2.generation_id, 2);

        // While Gen 2 is building, insert unit B in Gen 2
        let rec_b = EmbeddingRecord {
            retrieval_unit_id: "unit_b".to_string(),
            repository_id: "repo1".to_string(),
            source_revision_id: "s1".to_string(),
            index_generation_id: "i1".to_string(),
            selection_version: "v1".to_string(),
            provider_id: "qwen3".to_string(),
            model_id: "qwen3-0.6b".to_string(),
            content_hash: "hash_b".to_string(),
            dim: 2,
            vector: vec![0.0, 1.0],
        };
        store.put_batch_for_generation(&[rec_b], 2).unwrap();

        // Query Gen 1 again: MUST NOT contain unit B (zero cross-space mixing!)
        let res1_again = store
            .knn_search_generation(1, &[0.0, 1.0], 5, None, &budget)
            .unwrap();
        assert_eq!(res1_again.hits.len(), 1);
        assert_eq!(res1_again.hits[0].retrieval_unit_id, "unit_a");

        // Activate Gen 2 atomically
        store.activate_generation(2).unwrap();
        let active_gen = store.get_active_generation().unwrap().unwrap();
        assert_eq!(active_gen.generation_id, 2);

        // Query Gen 2
        let res2 = store
            .knn_search_generation(2, &[0.0, 1.0], 5, None, &budget)
            .unwrap();
        assert_eq!(res2.hits.len(), 1);
        assert_eq!(res2.hits[0].retrieval_unit_id, "unit_b");

        // Roll back to Gen 1
        let rolled_back = store.rollback_generation().unwrap().unwrap();
        assert_eq!(rolled_back.generation_id, 1);
        let active_after_rollback = store.get_active_generation().unwrap().unwrap();
        assert_eq!(active_after_rollback.generation_id, 1);
    }

    #[test]
    fn learned_tuning_store_roundtrip_and_invalidation() {
        let store = SemanticStore::open_in_memory().unwrap();
        let key = crate::learned_tuning::TuningKey {
            cpu_architecture: "x86_64".into(),
            os_name: "windows".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: "pinned_sha".into(),
            dimension: 1024,
            runtime_version: "0.1.0".into(),
        };

        // None initially
        assert!(store.read_learned_tuning(&key).unwrap().is_none());

        // Save tuning
        let rec = crate::learned_tuning::LearnedTuningRecord::new(key.clone(), 4, 32, 4, 210.0);
        store.save_learned_tuning(&rec).unwrap();

        // Read back
        let read = store.read_learned_tuning(&key).unwrap().unwrap();
        assert_eq!(read.recommended_lanes, 4);
        assert_eq!(read.recommended_batch_size, 32);
        assert_eq!(read.recommended_cpu_threads, 4);
        assert!((read.observed_chunks_per_sec - 210.0).abs() < 1e-3);

        // Invalidate
        let invalidated = store.invalidate_learned_tuning(&key).unwrap();
        assert!(invalidated);
        assert!(store.read_learned_tuning(&key).unwrap().is_none());
    }

    #[test]
    fn empty_db_initializes_exact_final_schema() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_semantic.db");

        // Opening fresh DB must apply the single squashed 0001_initial migration
        let store = SemanticStore::open(&db_path).expect("open fresh semantic store");
        let conn = store.guard().unwrap();

        // 1. Verify all expected tables exist
        let expected_tables = [
            "sem_schema_migrations",
            "sem_embeddings",
            "sem_queue",
            "sem_query_demand",
            "sem_generations",
            "sem_learned_tuning",
        ];
        for tbl in expected_tables {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name = ?1",
                    params![tbl],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(
                exists,
                "table '{tbl}' must exist in fresh semantic database"
            );
        }

        // Verify legacy profile table is absent
        let legacy_profile_exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name = 'sem_embedding_profile'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            !legacy_profile_exists,
            "legacy 'sem_embedding_profile' table must NOT exist in fresh semantic database"
        );

        // 2. Verify all expected indexes exist
        let expected_indexes = [
            "idx_sem_model",
            "idx_sem_embeddings_gen",
            "idx_sem_embeddings_gen_repo",
            "idx_sem_embeddings_model_repo",
            "idx_sem_queue_state",
            "idx_sem_gen_status",
        ];
        for idx in expected_indexes {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='index' AND name = ?1",
                    params![idx],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(
                exists,
                "index '{idx}' must exist in fresh semantic database"
            );
        }

        // 3. Verify exactly one baseline migration is recorded
        let migration_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sem_schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(migration_count, 1);

        let migration_id: String = conn
            .query_row("SELECT id FROM sem_schema_migrations LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(migration_id, "0001_initial");
    }

    #[test]
    fn reopening_same_final_db_is_idempotent() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test_semantic_idempotent.db");

        // First open
        {
            let store = SemanticStore::open(&db_path).expect("first open");
            let conn = store.guard().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM sem_schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 1);
        }

        // Second open on existing database
        {
            let store =
                SemanticStore::open(&db_path).expect("second open must succeed idempotently");
            let conn = store.guard().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM sem_schema_migrations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(count, 1);
        }
    }
}
