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
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::SemanticError;
use crate::generation::{GenerationManager, GenerationRecord};
use crate::provider::{CancelFlag, EmbeddingFingerprint};
use hnsw_rs::prelude::*;
use rusqlite::{Connection, params};

const SEMANTIC_BASELINE_SQL: &str = include_str!("../../../migrations/semantic/0001_initial.sql");
/// Id recorded in `sem_schema_migrations` by the baseline.
const SEMANTIC_BASELINE_VERSION: &str = "0001_initial";

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

/// Queue row states (`sem_queue_v2.state`).
pub const Q_PENDING: &str = "PENDING";
pub const Q_INFLIGHT: &str = "INFLIGHT";
pub const Q_DONE: &str = "DONE";
pub const Q_FAILED: &str = "FAILED";

/// Queue depth snapshot (`sem_queue_v2`), for status and ETA reporting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub pending: u64,
    pub inflight: u64,
    pub done: u64,
    pub failed: u64,
}

/// Full lineage/provenance of one occurrence of canonical content, as
/// registered by [`SemanticStore::add_occurrence`] and read back by a drive
/// loop via [`SemanticStore::occurrence_by_id`].
#[derive(Debug, Clone)]
pub struct OccurrenceRecord {
    pub occurrence_id: String,
    pub retrieval_unit_id: String,
    pub vector_space_id: String,
    pub canonical_hash: String,
    pub repository_id: String,
    pub source_revision_id: String,
    pub index_generation_id: String,
    pub content_generation_id: String,
    pub metadata_json: String,
}

/// One occurrence's completed embedding, ready for
/// [`SemanticStore::commit_batch`].
#[derive(Debug, Clone)]
pub struct CommitEntry {
    pub occurrence_id: String,
    pub owner: String,
    pub fencing_token: i64,
    pub retrieval_unit_id: String,
    pub repository_id: String,
    pub source_revision_id: String,
    pub index_generation_id: String,
    pub vector_space_id: String,
    pub canonical_hash: String,
    pub provider_id: String,
    pub model_id: String,
    pub vector: Vec<f32>,
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
        if let Some(&ann_id) = self.unit_to_ann_id.get(unit_id)
            && let Some(entry) = self.metadata.get_mut(&ann_id)
        {
            entry.deleted = true;
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

/// One occurrence row for [`SemanticStore::add_occurrences_and_enqueue`].
/// The occurrence id doubles as the retrieval unit id (one occurrence per
/// unit, see `invalidate::reconcile`).
pub struct NewOccurrence<'a> {
    pub occurrence_id: &'a str,
    pub vector_space_id: &'a str,
    pub canonical_hash: &'a str,
    pub repository_id: &'a str,
    pub source_revision_id: &'a str,
    pub index_generation_id: &'a str,
    pub content_generation_id: &'a str,
    /// `Some` enqueues the occurrence for embedding at this priority.
    pub enqueue_priority: Option<f64>,
}
/// Shared-handle-safe semantic store: rusqlite connections are `!Sync`, so
/// every access goes through an internal mutex (contention is negligible at
/// Phase 5 scales; queries hold it only for bounded reads).
#[derive(Debug)]
pub struct SemanticStore {
    conn: Mutex<Connection>,
    candidate_index: Mutex<HashMap<i64, GenerationIndex>>,
}

/// L2 norm plus a little-endian f32 byte blob — the on-disk vector encoding
/// shared by every embedding-storing insert (`sem_embeddings`/`sem_embeddings_v2`
/// both store the norm and the blob separately: the norm speeds up cosine
/// scoring without re-deriving it from the blob on every candidate).
fn encode_vector_blob(vector: &[f32]) -> (f32, Vec<u8>) {
    let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mut blob = Vec::with_capacity(vector.len() * 4);
    for v in vector {
        blob.extend_from_slice(&v.to_le_bytes());
    }
    (norm, blob)
}

impl SemanticStore {
    /// Open (creating if needed) the disposable semantic database.
    ///
    /// Work claimed by a process that died is recovered by lease expiry in
    /// the v2 queue (`queue_reclaim_expired`), not by a blanket reset
    /// here, so a concurrently running worker's live leases are never
    /// stolen.
    pub fn open(path: &Path) -> Result<Self, SemanticError> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::migrate(&conn)?;
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

    fn normalize_workspace_root_key(path: &Path) -> String {
        let s = path.to_string_lossy().replace('\\', "/");
        let s = s
            .trim_start_matches("//?/")
            .trim_end_matches('/')
            .to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    }

    fn ensure_workspace_membership_table(conn: &Connection) -> Result<(), SemanticError> {
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS sem_workspace_active_roots
                (root_key TEXT PRIMARY KEY);",
        )?;
        Ok(())
    }

    fn workspace_membership_contains(
        conn: &Connection,
        root_path: &Path,
    ) -> Result<bool, SemanticError> {
        Self::ensure_workspace_membership_table(conn)?;
        let root_key = Self::normalize_workspace_root_key(root_path);
        let mut stmt = conn.prepare("SELECT root_key FROM temp.sem_workspace_active_roots")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let configured_key: String = row.get(0)?;
            if root_key == configured_key || root_key.starts_with(&format!("{configured_key}/")) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Replace the semantic-store snapshot of active workspace roots.
    pub fn sync_workspace_membership(&self, active_roots: &[PathBuf]) -> Result<(), SemanticError> {
        let conn = self.guard()?;
        Self::ensure_workspace_membership_table(&conn)?;
        conn.execute("DELETE FROM temp.sem_workspace_active_roots", [])?;
        let mut insert = conn.prepare(
            "INSERT OR REPLACE INTO temp.sem_workspace_active_roots (root_key) VALUES (?1)",
        )?;
        for root in active_roots {
            insert.execute(params![Self::normalize_workspace_root_key(root)])?;
        }
        Ok(())
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

    /// Apply the single baseline. `semantic.db` is disposable, so a database
    /// created by any other schema is wiped and rebuilt (its vectors are
    /// re-embedded in the background).
    fn migrate(conn: &Connection) -> Result<(), SemanticError> {
        if !Self::is_current_schema(conn)? {
            tracing::warn!(
                "semantic.db was created by a different schema; rebuilding it (embeddings are regenerated)"
            );
            attic_storage::migration::reset_schema(conn)?;
        }
        conn.execute_batch(SEMANTIC_BASELINE_SQL)?;
        Ok(())
    }

    /// `true` for a fresh database or one created by exactly this baseline.
    fn is_current_schema(conn: &Connection) -> Result<bool, SemanticError> {
        let table_count = |name: &str| -> Result<i64, SemanticError> {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [name],
                |r| r.get(0),
            )?)
        };
        if table_count("sem_schema_migrations")? == 0 {
            let tables: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )?;
            return Ok(tables == 0);
        }
        let mut stmt = conn.prepare("SELECT id FROM sem_schema_migrations")?;
        let applied: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        // An earlier baseline carried the same id plus the v1 `sem_queue`.
        Ok(applied == [SEMANTIC_BASELINE_VERSION] && table_count("sem_queue")? == 0)
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    // ── embeddings ─────────────────────────────────────────────────────────

    /// Test support: insert or replace one per-generation projection row.
    #[cfg(test)]
    fn put(&self, rec: &EmbeddingRecord) -> Result<(), SemanticError> {
        let (norm, blob) = encode_vector_blob(&rec.vector);
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

    // ── canonical embeddings, occurrences, leases ──────────────────────────

    /// Store one canonical embedding; returns true when newly inserted
    /// (INSERT OR IGNORE — an identical canonical body in the same vector
    /// space is embedded exactly once, ever).
    pub fn put_canonical_embedding(
        &self,
        vector_space_id: &str,
        canonical_hash: &str,
        vector: &[f32],
    ) -> Result<bool, SemanticError> {
        let (norm, blob) = encode_vector_blob(vector);
        let changed = self.guard()?.execute(
            "INSERT OR IGNORE INTO sem_embeddings_v2
                 (vector_space_id, canonical_hash, dim, norm, vector, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                vector_space_id,
                canonical_hash,
                vector.len() as i64,
                norm,
                blob,
                Self::now_ms()
            ],
        )?;
        Ok(changed > 0)
    }

    /// Fetch the canonical embedding for reuse across occurrences.
    /// Every canonical hash that already has a vector in `vector_space_id`.
    pub fn canonical_hashes_with_vectors(
        &self,
        vector_space_id: &str,
    ) -> Result<std::collections::HashSet<String>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn
            .prepare("SELECT canonical_hash FROM sem_embeddings_v2 WHERE vector_space_id = ?1")?;
        let mut rows = stmt.query(params![vector_space_id])?;
        let mut out = std::collections::HashSet::new();
        while let Some(r) = rows.next()? {
            out.insert(r.get(0)?);
        }
        Ok(out)
    }

    pub fn embedding_for_canonical(
        &self,
        vector_space_id: &str,
        canonical_hash: &str,
    ) -> Result<Option<Vec<f32>>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT dim, vector FROM sem_embeddings_v2
              WHERE vector_space_id = ?1 AND canonical_hash = ?2",
        )?;
        let mut rows = stmt.query(params![vector_space_id, canonical_hash])?;
        if let Some(row) = rows.next()? {
            let dim = row.get::<_, i64>(0)? as usize;
            let blob: Vec<u8> = row.get(1)?;
            if blob.len() != dim * 4 {
                return Err(SemanticError::StoreUnavailable(format!(
                    "corrupt sem_embeddings_v2 row: dim {dim} but {} blob bytes",
                    blob.len()
                )));
            }
            let mut out = Vec::with_capacity(dim);
            for chunk in blob.as_chunks::<4>().0 {
                out.push(f32::from_le_bytes(*chunk));
            }
            return Ok(Some(out));
        }
        Ok(None)
    }

    /// Record one occurrence of canonical content (all fields are lineage /
    /// provenance; the vector itself lives once in sem_embeddings_v2).
    #[allow(clippy::too_many_arguments)]
    pub fn add_occurrence(
        &self,
        occurrence_id: &str,
        retrieval_unit_id: &str,
        vector_space_id: &str,
        canonical_hash: &str,
        repository_id: &str,
        source_revision_id: &str,
        index_generation_id: &str,
        content_generation_id: &str,
        metadata_json: &str,
    ) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "INSERT OR REPLACE INTO sem_embedding_occurrences
                 (occurrence_id, retrieval_unit_id, vector_space_id,
                  canonical_hash, repository_id, source_revision_id,
                  index_generation_id, content_generation_id, metadata_json,
                  created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                occurrence_id,
                retrieval_unit_id,
                vector_space_id,
                canonical_hash,
                repository_id,
                source_revision_id,
                index_generation_id,
                content_generation_id,
                metadata_json,
                Self::now_ms()
            ],
        )?;
        Ok(())
    }

    /// Record many occurrences (and queue the ones with a priority) in ONE
    /// transaction with prepared statements. Same rows as calling
    /// [`Self::add_occurrence`] + [`Self::queue_enqueue`] per unit, which
    /// autocommitted each row and dominated reconcile time on large repos.
    pub fn add_occurrences_and_enqueue(
        &self,
        records: &[NewOccurrence<'_>],
    ) -> Result<usize, SemanticError> {
        if records.is_empty() {
            return Ok(0);
        }
        let conn = self.guard()?;
        let tx = conn.unchecked_transaction()?;
        let mut newly_enqueued = 0usize;
        {
            let now = Self::now_ms();
            let mut occ = tx.prepare(
                "INSERT OR REPLACE INTO sem_embedding_occurrences
                     (occurrence_id, retrieval_unit_id, vector_space_id,
                      canonical_hash, repository_id, source_revision_id,
                      index_generation_id, content_generation_id, metadata_json,
                      created_at_ms)
                 VALUES (?1,?1,?2,?3,?4,?5,?6,?7,'{}',?8)",
            )?;
            let mut queue = tx.prepare(
                "INSERT OR IGNORE INTO sem_queue_v2
                     (occurrence_id, priority, state, attempts, enqueued_at_ms)
                 VALUES (?1,?2,'PENDING',0,?3)",
            )?;
            for r in records {
                occ.execute(params![
                    r.occurrence_id,
                    r.vector_space_id,
                    r.canonical_hash,
                    r.repository_id,
                    r.source_revision_id,
                    r.index_generation_id,
                    r.content_generation_id,
                    now
                ])?;
                if let Some(priority) = r.enqueue_priority {
                    newly_enqueued += queue.execute(params![r.occurrence_id, priority, now])?;
                }
            }
        }
        tx.commit()?;
        Ok(newly_enqueued)
    }

    /// `retrieval_unit_id -> canonical_hash` for every occurrence registered
    /// in one vector space (lets reconcile skip rewriting unchanged rows).
    pub fn occurrence_hashes(
        &self,
        vector_space_id: &str,
    ) -> Result<std::collections::HashMap<String, String>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT retrieval_unit_id, canonical_hash FROM sem_embedding_occurrences
              WHERE vector_space_id = ?1",
        )?;
        let mut rows = stmt.query(params![vector_space_id])?;
        let mut out = std::collections::HashMap::new();
        while let Some(r) = rows.next()? {
            out.insert(r.get(0)?, r.get(1)?);
        }
        Ok(out)
    }

    /// Delete the per-generation projection rows of `unit_ids` for one model
    /// in ONE transaction (reconcile's stale set can be tens of thousands of
    /// rows; one autocommit per row held the store mutex for ~1 minute), then
    /// recount the affected generations and drop the units from the
    /// in-memory candidate index. Returns rows deleted.
    pub fn delete_projections(
        &self,
        unit_ids: &[String],
        provider: &str,
        model: &str,
    ) -> Result<usize, SemanticError> {
        if unit_ids.is_empty() {
            return Ok(0);
        }
        let mut conn = self.guard()?;
        let tx = conn.transaction()?;
        let mut n = 0usize;
        {
            let mut del = tx.prepare(
                "DELETE FROM sem_embeddings
                  WHERE retrieval_unit_id = ?1 AND provider_id = ?2 AND model_id = ?3",
            )?;
            for id in unit_ids {
                n += del.execute(params![id, provider, model])?;
            }
        }
        if n > 0 {
            tx.execute(
                "UPDATE sem_generations SET unit_count =
                    (SELECT COUNT(*) FROM sem_embeddings e
                      WHERE e.generation_id = sem_generations.generation_id)",
                [],
            )?;
        }
        tx.commit()?;
        drop(conn);
        if n > 0 {
            self.tombstone_units_in_all_indexes(unit_ids.iter().map(String::as_str));
        }
        Ok(n)
    }

    /// Remove occurrences in `vector_space_id` whose unit is not in `keep`
    /// (the unit left the canonical index or is no longer selected/linked),
    /// except ones a queue row still references (an INFLIGHT lease completes
    /// or releases on its own). Without this, every vanished unit's
    /// occurrence was re-projected after each drive slice and deleted again
    /// by the next reconcile, forever. Returns occurrences removed.
    pub fn prune_occurrences(
        &self,
        vector_space_id: &str,
        keep: &std::collections::HashSet<&str>,
    ) -> Result<usize, SemanticError> {
        let conn = self.guard()?;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS sem_keep_occ (id TEXT PRIMARY KEY);
             DELETE FROM temp.sem_keep_occ;",
        )?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO temp.sem_keep_occ (id) VALUES (?1)")?;
            for id in keep {
                ins.execute(params![id])?;
            }
        }
        let n = tx.execute(
            "DELETE FROM sem_embedding_occurrences
              WHERE vector_space_id = ?1
                AND retrieval_unit_id NOT IN (SELECT id FROM temp.sem_keep_occ)
                AND NOT EXISTS (SELECT 1 FROM sem_queue_v2 q
                                 WHERE q.occurrence_id = sem_embedding_occurrences.occurrence_id)",
            params![vector_space_id],
        )?;
        tx.execute("DELETE FROM temp.sem_keep_occ", [])?;
        tx.commit()?;
        Ok(n)
    }
    /// Count occurrences sharing one canonical embedding — the dedup proof.
    pub fn occurrence_count_for_canonical(
        &self,
        vector_space_id: &str,
        canonical_hash: &str,
    ) -> Result<u64, SemanticError> {
        let n: i64 = self.guard()?.query_row(
            "SELECT COUNT(*) FROM sem_embedding_occurrences
              WHERE vector_space_id = ?1 AND canonical_hash = ?2",
            params![vector_space_id, canonical_hash],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Enqueue an occurrence for embedding (idempotent; preserves state of an
    /// already-queued row).
    pub fn queue_enqueue(&self, occurrence_id: &str, priority: f64) -> Result<(), SemanticError> {
        self.guard()?.execute(
            "INSERT OR IGNORE INTO sem_queue_v2
                 (occurrence_id, priority, state, attempts, enqueued_at_ms)
             VALUES (?1,?2,'PENDING',0,?3)",
            params![occurrence_id, priority, Self::now_ms()],
        )?;
        Ok(())
    }

    /// Claim one pending (or lease-expired) occurrence for `owner`. Bumps the
    /// fencing token so any previous owner's later writes are stale.
    /// Returns the fencing token the caller MUST present on completion.
    pub fn queue_claim(
        &self,
        occurrence_id: &str,
        owner: &str,
        lease_ms: i64,
    ) -> Result<Option<i64>, SemanticError> {
        let now = Self::now_ms();
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET state = 'INFLIGHT', lease_owner = ?2,
                    lease_expires_at_ms = ?3, heartbeat_at_ms = ?4,
                    fencing_token = fencing_token + 1
              WHERE occurrence_id = ?1
                AND (state = 'PENDING'
                     OR (state = 'INFLIGHT' AND lease_expires_at_ms < ?4))",
            params![occurrence_id, owner, now + lease_ms, now],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        let token: i64 = self.guard()?.query_row(
            "SELECT fencing_token FROM sem_queue_v2 WHERE occurrence_id = ?1",
            params![occurrence_id],
            |r| r.get(0),
        )?;
        Ok(Some(token))
    }

    /// Extend a lease; rejected (Ok(false)) when the presented fencing token
    /// is stale — i.e. the row was reclaimed by another worker.
    pub fn queue_heartbeat(
        &self,
        occurrence_id: &str,
        owner: &str,
        fencing_token: i64,
        lease_ms: i64,
    ) -> Result<bool, SemanticError> {
        let now = Self::now_ms();
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET lease_expires_at_ms = ?4, heartbeat_at_ms = ?5
              WHERE occurrence_id = ?1 AND lease_owner = ?2
                AND fencing_token = ?3 AND state = 'INFLIGHT'",
            params![occurrence_id, owner, fencing_token, now + lease_ms, now],
        )?;
        Ok(changed > 0)
    }

    /// Mark an occurrence done; rejected (Ok(false)) on a stale fencing
    /// token, so a killed worker can never commit after reclaim.
    pub fn queue_complete(
        &self,
        occurrence_id: &str,
        owner: &str,
        fencing_token: i64,
    ) -> Result<bool, SemanticError> {
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET state = 'DONE', lease_owner = NULL, lease_expires_at_ms = NULL
              WHERE occurrence_id = ?1 AND lease_owner = ?2
                AND fencing_token = ?3 AND state = 'INFLIGHT'",
            params![occurrence_id, owner, fencing_token],
        )?;
        Ok(changed > 0)
    }

    /// Crash/restart reclaim: expired INFLIGHT leases become PENDING again
    /// with an incremented attempt counter. Returns rows reclaimed.
    pub fn queue_reclaim_expired(&self) -> Result<u64, SemanticError> {
        let now = Self::now_ms();
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET state = 'PENDING', attempts = attempts + 1,
                    lease_owner = NULL, lease_expires_at_ms = NULL
              WHERE state = 'INFLIGHT' AND lease_expires_at_ms < ?1",
            params![now],
        )?;
        Ok(changed as u64)
    }

    /// Claim up to `limit` occurrences for `owner` in one call: selects
    /// PENDING (or lease-expired INFLIGHT) candidates, then claims each
    /// through the same atomic single-row UPDATE as [`Self::queue_claim`]
    /// — the candidate scan is just discovery; exclusivity is enforced by
    /// that per-row UPDATE, so a candidate already claimed by a concurrent
    /// drive loop is silently skipped rather than double-assigned.
    pub fn queue_claim_batch(
        &self,
        owner: &str,
        lease_ms: i64,
        limit: usize,
    ) -> Result<Vec<(String, i64)>, SemanticError> {
        let now = Self::now_ms();
        let candidate_ids: Vec<String> = {
            let conn = self.guard()?;
            let mut stmt = conn.prepare(
                "SELECT occurrence_id FROM sem_queue_v2
                  WHERE state = 'PENDING'
                     OR (state = 'INFLIGHT' AND lease_expires_at_ms < ?1)
                  ORDER BY priority DESC, enqueued_at_ms ASC
                  LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![now, limit as i64], |r| r.get::<_, String>(0))?;
            rows.filter_map(Result::ok).collect()
        };
        let mut claimed = Vec::with_capacity(candidate_ids.len());
        for occ_id in candidate_ids {
            if let Some(token) = self.queue_claim(&occ_id, owner, lease_ms)? {
                claimed.push((occ_id, token));
            }
        }
        Ok(claimed)
    }

    /// Return a claimed occurrence to PENDING without incrementing attempts
    /// (cancellation, transient pre-embed failure) — rejected on a stale
    /// fencing token exactly like [`Self::queue_complete`].
    pub fn queue_reset(
        &self,
        occurrence_id: &str,
        owner: &str,
        fencing_token: i64,
    ) -> Result<bool, SemanticError> {
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET state = 'PENDING', lease_owner = NULL, lease_expires_at_ms = NULL
              WHERE occurrence_id = ?1 AND lease_owner = ?2
                AND fencing_token = ?3 AND state = 'INFLIGHT'",
            params![occurrence_id, owner, fencing_token],
        )?;
        Ok(changed > 0)
    }

    /// Record a failed attempt; quarantines as FAILED once `max_attempts` is
    /// reached, otherwise returns to PENDING for retry at the BACK of its
    /// priority band (`enqueued_at_ms = now`), so a repeatedly failing item
    /// can never sit at the head of the queue and be re-claimed ahead of
    /// healthy work. Rejected on a stale fencing token so a superseded
    /// worker can never affect queue state.
    pub fn queue_mark_failed(
        &self,
        occurrence_id: &str,
        owner: &str,
        fencing_token: i64,
        max_attempts: u32,
        error: &str,
    ) -> Result<bool, SemanticError> {
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET attempts = attempts + 1,
                    state = CASE WHEN attempts + 1 >= ?4 THEN 'FAILED' ELSE 'PENDING' END,
                    lease_owner = NULL, lease_expires_at_ms = NULL,
                    last_error = ?5, enqueued_at_ms = ?6
              WHERE occurrence_id = ?1 AND lease_owner = ?2
                AND fencing_token = ?3 AND state = 'INFLIGHT'",
            params![
                occurrence_id,
                owner,
                fencing_token,
                max_attempts as i64,
                error,
                Self::now_ms()
            ],
        )?;
        Ok(changed > 0)
    }

    /// Quarantine an occurrence immediately regardless of attempt count
    /// (secret-bearing content, oversized input, dropped-input, dimension
    /// mismatch — permanent for this content, never worth retrying).
    pub fn queue_fail_permanently(
        &self,
        occurrence_id: &str,
        owner: &str,
        fencing_token: i64,
        error: &str,
    ) -> Result<bool, SemanticError> {
        let changed = self.guard()?.execute(
            "UPDATE sem_queue_v2
                SET state = 'FAILED', lease_owner = NULL, lease_expires_at_ms = NULL,
                    last_error = ?4
              WHERE occurrence_id = ?1 AND lease_owner = ?2
                AND fencing_token = ?3 AND state = 'INFLIGHT'",
            params![occurrence_id, owner, fencing_token, error],
        )?;
        Ok(changed > 0)
    }

    /// The full lineage/provenance an occurrence carries — what a drive loop
    /// needs to load canonical text and commit a completed embedding.
    pub fn occurrence_by_id(
        &self,
        occurrence_id: &str,
    ) -> Result<Option<OccurrenceRecord>, SemanticError> {
        let conn = self.guard()?;
        let mut stmt = conn.prepare(
            "SELECT occurrence_id, retrieval_unit_id, vector_space_id, canonical_hash,
                    repository_id, source_revision_id, index_generation_id,
                    content_generation_id, metadata_json
               FROM sem_embedding_occurrences WHERE occurrence_id = ?1",
        )?;
        let mut rows = stmt.query(params![occurrence_id])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(OccurrenceRecord {
                occurrence_id: row.get(0)?,
                retrieval_unit_id: row.get(1)?,
                vector_space_id: row.get(2)?,
                canonical_hash: row.get(3)?,
                repository_id: row.get(4)?,
                source_revision_id: row.get(5)?,
                index_generation_id: row.get(6)?,
                content_generation_id: row.get(7)?,
                metadata_json: row.get(8)?,
            }));
        }
        Ok(None)
    }

    /// Batched form of [`Self::occurrence_by_id`] — one query for an entire
    /// claimed batch instead of one round-trip per occurrence (mirrors
    /// `attic_storage::semantic_units_by_ids`'s batching for the same set of
    /// IDs, used right alongside this in `enrich::drive_leased`).
    pub fn occurrences_by_ids(
        &self,
        occurrence_ids: &[String],
    ) -> Result<HashMap<String, OccurrenceRecord>, SemanticError> {
        let mut out = HashMap::with_capacity(occurrence_ids.len());
        if occurrence_ids.is_empty() {
            return Ok(out);
        }
        let conn = self.guard()?;
        let placeholders = vec!["?"; occurrence_ids.len()].join(",");
        let sql = format!(
            "SELECT occurrence_id, retrieval_unit_id, vector_space_id, canonical_hash,
                    repository_id, source_revision_id, index_generation_id,
                    content_generation_id, metadata_json
               FROM sem_embedding_occurrences WHERE occurrence_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let paramslice: Vec<&dyn rusqlite::ToSql> = occurrence_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();
        let mut rows = stmt.query(paramslice.as_slice())?;
        while let Some(row) = rows.next()? {
            let rec = OccurrenceRecord {
                occurrence_id: row.get(0)?,
                retrieval_unit_id: row.get(1)?,
                vector_space_id: row.get(2)?,
                canonical_hash: row.get(3)?,
                repository_id: row.get(4)?,
                source_revision_id: row.get(5)?,
                index_generation_id: row.get(6)?,
                content_generation_id: row.get(7)?,
                metadata_json: row.get(8)?,
            };
            out.insert(rec.occurrence_id.clone(), rec);
        }
        Ok(out)
    }

    /// Transactionally commit a batch of completed v2 occurrences: insert
    /// each distinct canonical vector at most once, complete the v2 queue
    /// row (fencing-checked — a stale entry is silently dropped from the
    /// commit, not an error, since another worker legitimately owns it now),
    /// and project the same vector into the existing per-generation
    /// `sem_embeddings` retrieval table so the already-proven HNSW candidate
    /// index keeps serving queries unchanged while gaining full occurrence
    /// coverage (every occurrence gets its own retrievable row, including
    /// ones a canonical-dedup pass would otherwise have dropped silently).
    /// Returns the occurrence ids actually committed.
    pub fn commit_batch(
        &self,
        entries: &[CommitEntry],
        generation_id: i64,
        selection_version: &str,
    ) -> Result<Vec<String>, SemanticError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.guard()?;
        let tx = conn.transaction()?;
        let now = Self::now_ms();
        let mut committed = Vec::with_capacity(entries.len());
        {
            let mut insert_v2 = tx.prepare(
                "INSERT OR IGNORE INTO sem_embeddings_v2
                     (vector_space_id, canonical_hash, dim, norm, vector, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6)",
            )?;
            let mut complete_stmt = tx.prepare(
                "UPDATE sem_queue_v2
                    SET state = 'DONE', lease_owner = NULL, lease_expires_at_ms = NULL
                  WHERE occurrence_id = ?1 AND lease_owner = ?2
                    AND fencing_token = ?3 AND state = 'INFLIGHT'",
            )?;
            let mut project_v1 = tx.prepare(
                "INSERT OR REPLACE INTO sem_embeddings
                     (retrieval_unit_id, repository_id, source_revision_id,
                      index_generation_id, selection_version, provider_id, model_id,
                      content_hash, dim, norm, vector, created_at_ms, generation_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            let mut check_new_v1 = tx.prepare(
                "SELECT 1 FROM sem_embeddings WHERE retrieval_unit_id=?1 AND provider_id=?2 AND model_id=?3 AND generation_id=?4"
            )?;

            let mut new_units = 0i64;
            for e in entries {
                let changed =
                    complete_stmt.execute(params![e.occurrence_id, e.owner, e.fencing_token])?;
                if changed == 0 {
                    // Stale fencing token: another owner reclaimed this
                    // occurrence. Not this commit's to make.
                    continue;
                }

                let (norm, blob) = encode_vector_blob(&e.vector);
                insert_v2.execute(params![
                    e.vector_space_id,
                    e.canonical_hash,
                    e.vector.len() as i64,
                    norm,
                    blob,
                    now
                ])?;

                if !check_new_v1.exists(params![
                    e.retrieval_unit_id,
                    e.provider_id,
                    e.model_id,
                    generation_id
                ])? {
                    new_units += 1;
                }
                project_v1.execute(params![
                    e.retrieval_unit_id,
                    e.repository_id,
                    e.source_revision_id,
                    e.index_generation_id,
                    selection_version,
                    e.provider_id,
                    e.model_id,
                    e.canonical_hash,
                    e.vector.len() as i64,
                    norm,
                    blob,
                    now,
                    generation_id,
                ])?;
                committed.push(e.occurrence_id.clone());
            }

            if new_units > 0 {
                tx.execute(
                    "UPDATE sem_generations SET unit_count = unit_count + ?1 WHERE generation_id = ?2",
                    params![new_units, generation_id],
                )?;
            }
        }
        tx.commit()?;
        Ok(committed)
    }

    /// Project every occurrence whose canonical vector already exists in
    /// `sem_embeddings_v2` but has no row yet in the per-generation
    /// `sem_embeddings` projection. Closes the gap left by canonical dedup:
    /// a duplicate-content occurrence is deliberately never queued (only
    /// the winning occurrence's completion produces the vector — see
    /// `invalidate::reconcile`), and it may be registered before OR after
    /// that vector exists, so this catch-up pass is what actually makes it
    /// retrievable, regardless of timing. Returns rows projected.
    #[allow(clippy::type_complexity)]
    pub fn project_resolved_orphan_occurrences(
        &self,
        generation_id: i64,
        provider_id: &str,
        model_id: &str,
        selection_version: &str,
    ) -> Result<u64, SemanticError> {
        let mut conn = self.guard()?;
        let tx = conn.transaction()?;
        let mut projected = 0u64;
        {
            let orphans: Vec<(
                String,
                String,
                String,
                String,
                String,
                String,
                usize,
                f32,
                Vec<u8>,
            )> = {
                let mut stmt = tx.prepare(
                    "SELECT o.retrieval_unit_id, o.repository_id, o.source_revision_id,
                            o.index_generation_id, o.vector_space_id, o.canonical_hash,
                            v.dim, v.norm, v.vector
                       FROM sem_embedding_occurrences o
                       JOIN sem_embeddings_v2 v
                         ON v.vector_space_id = o.vector_space_id
                        AND v.canonical_hash = o.canonical_hash
                  LEFT JOIN sem_embeddings se
                         ON se.retrieval_unit_id = o.retrieval_unit_id
                        AND se.provider_id = ?1 AND se.model_id = ?2
                        AND se.generation_id = ?3
                      WHERE se.retrieval_unit_id IS NULL",
                )?;
                let mut rows = stmt.query(params![provider_id, model_id, generation_id])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get::<_, i64>(6)? as usize,
                        row.get(7)?,
                        row.get(8)?,
                    ));
                }
                out
            };
            if !orphans.is_empty() {
                let mut project_v1 = tx.prepare(
                    "INSERT OR REPLACE INTO sem_embeddings
                         (retrieval_unit_id, repository_id, source_revision_id,
                          index_generation_id, selection_version, provider_id, model_id,
                          content_hash, dim, norm, vector, created_at_ms, generation_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                )?;
                let now = Self::now_ms();
                for (unit_id, repo_id, rev_id, idx_gen_id, _vsid, hash, dim, norm, vector) in
                    orphans
                {
                    project_v1.execute(params![
                        unit_id,
                        repo_id,
                        rev_id,
                        idx_gen_id,
                        selection_version,
                        provider_id,
                        model_id,
                        hash,
                        dim as i64,
                        norm,
                        vector,
                        now,
                        generation_id,
                    ])?;
                    projected += 1;
                }
                if projected > 0 {
                    tx.execute(
                        "UPDATE sem_generations SET unit_count = unit_count + ?1 WHERE generation_id = ?2",
                        params![projected as i64, generation_id],
                    )?;
                }
            }
        }
        tx.commit()?;
        Ok(projected)
    }

    /// Queue depth snapshot for status reporting.
    pub fn queue_counts(&self) -> Result<QueueCounts, SemanticError> {
        let conn = self.guard()?;
        let count = |state: &str| -> Result<u64, SemanticError> {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sem_queue_v2 WHERE state = ?1",
                params![state],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        };
        Ok(QueueCounts {
            pending: count(Q_PENDING)?,
            inflight: count(Q_INFLIGHT)?,
            done: count(Q_DONE)?,
            failed: count(Q_FAILED)?,
        })
    }

    /// Queue depth scoped to ONE vector space — distinct from
    /// [`Self::queue_counts`] (global), this answers "is there still
    /// outstanding work for backend X's vector space" so a caller (e.g.
    /// GPU→CPU fallback) can tell a BUILDING generation for a specific
    /// fingerprint is fully drained (and actually made progress) before
    /// treating it as activation-ready. Joins through
    /// `sem_embedding_occurrences` because `sem_queue_v2` itself is keyed by
    /// `occurrence_id`, not vector space.
    pub fn queue_counts_for_vector_space(
        &self,
        vector_space_id: &str,
    ) -> Result<QueueCounts, SemanticError> {
        let conn = self.guard()?;
        let count = |state: &str| -> Result<u64, SemanticError> {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM sem_queue_v2 q
                   JOIN sem_embedding_occurrences o ON o.occurrence_id = q.occurrence_id
                  WHERE q.state = ?1 AND o.vector_space_id = ?2",
                params![state, vector_space_id],
                |r| r.get(0),
            )?;
            Ok(n as u64)
        };
        Ok(QueueCounts {
            pending: count(Q_PENDING)?,
            inflight: count(Q_INFLIGHT)?,
            done: count(Q_DONE)?,
            failed: count(Q_FAILED)?,
        })
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
            self.candidate_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        Ok(n)
    }

    /// Remove every semantic row belonging to a repository removed from the
    /// workspace: queue entries, occurrences, per-generation projection rows,
    /// and canonical vectors no remaining occurrence references. Idempotent.
    /// Returns the number of rows deleted.
    pub fn evict_repository(&self, repository_id: &str) -> Result<usize, SemanticError> {
        let mut conn = self.guard()?;
        let tx = conn.transaction()?;
        let (n, projections) = Self::evict_repository_tx(&tx, repository_id)?;
        tx.commit()?;
        drop(conn);
        if projections > 0 {
            // The in-memory candidate index only syncs forward by rowid; drop
            // it so it is rebuilt without the evicted repository.
            self.candidate_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        Ok(n)
    }

    /// Delete semantic rows for `repository_id` only when `root_path` is not
    /// active in the current semantic-store membership snapshot. Returns
    /// `None` when a re-add raced in before this transaction began.
    pub fn evict_repository_if_inactive(
        &self,
        repository_id: &str,
        root_path: &Path,
    ) -> Result<Option<usize>, SemanticError> {
        let mut conn = self.guard()?;
        let tx = conn.transaction()?;
        if Self::workspace_membership_contains(&tx, root_path)? {
            return Ok(None);
        }
        let (n, projections) = Self::evict_repository_tx(&tx, repository_id)?;
        tx.commit()?;
        drop(conn);
        if projections > 0 {
            self.candidate_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
        Ok(Some(n))
    }

    fn evict_repository_tx(
        conn: &Connection,
        repository_id: &str,
    ) -> Result<(usize, usize), SemanticError> {
        // One transaction, and every statement scoped to this repository's
        // rows (no whole-table scans), so the store mutex is held briefly and
        // a crash leaves either nothing or everything deleted. Deleting the
        // queue rows also fences out any batch for this repo already
        // in flight: `commit_batch` only completes a queue row that still
        // exists with the presented fencing token.
        conn.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS evict_hashes
                (vector_space_id TEXT NOT NULL, canonical_hash TEXT NOT NULL,
                 PRIMARY KEY (vector_space_id, canonical_hash));
             CREATE TEMP TABLE IF NOT EXISTS evict_generations
                (generation_id INTEGER PRIMARY KEY);
             DELETE FROM evict_hashes;
             DELETE FROM evict_generations;",
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO evict_hashes
                SELECT vector_space_id, canonical_hash
                  FROM sem_embedding_occurrences WHERE repository_id = ?1",
            params![repository_id],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO evict_generations
                SELECT DISTINCT generation_id FROM sem_embeddings
                 WHERE repository_id = ?1 AND generation_id IS NOT NULL",
            params![repository_id],
        )?;
        let mut n = conn.execute(
            "DELETE FROM sem_queue_v2 WHERE occurrence_id IN
                (SELECT occurrence_id FROM sem_embedding_occurrences WHERE repository_id = ?1)",
            params![repository_id],
        )?;
        n += conn.execute(
            "DELETE FROM sem_embedding_occurrences WHERE repository_id = ?1",
            params![repository_id],
        )?;
        let projections = conn.execute(
            "DELETE FROM sem_embeddings WHERE repository_id = ?1",
            params![repository_id],
        )?;
        n += projections;
        conn.execute(
            "UPDATE sem_generations SET unit_count =
                (SELECT COUNT(*) FROM sem_embeddings e
                  WHERE e.generation_id = sem_generations.generation_id)
              WHERE generation_id IN (SELECT generation_id FROM evict_generations)",
            [],
        )?;
        n += conn.execute(
            "DELETE FROM sem_embeddings_v2
              WHERE (vector_space_id, canonical_hash) IN
                    (SELECT vector_space_id, canonical_hash FROM evict_hashes)
                AND NOT EXISTS
                    (SELECT 1 FROM sem_embedding_occurrences o
                      WHERE o.vector_space_id = sem_embeddings_v2.vector_space_id
                        AND o.canonical_hash  = sem_embeddings_v2.canonical_hash)",
            [],
        )?;
        conn.execute_batch("DELETE FROM evict_hashes; DELETE FROM evict_generations;")?;
        Ok((n, projections))
    }

    /// Delete everything for one model (full semantic-layer reset).
    pub fn purge_model(&self, provider: &str, model: &str) -> Result<usize, SemanticError> {
        let n = self.guard()?.execute(
            "DELETE FROM sem_embeddings WHERE provider_id=?1 AND model_id=?2",
            params![provider, model],
        )?;
        if n > 0 {
            self.candidate_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
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

    /// Test support: insert a batch of projection rows tagged with a
    /// generation id (§52) and bump that generation's unit count.
    #[cfg(test)]
    fn put_batch_for_generation(
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

                let (norm, blob) = encode_vector_blob(&rec.vector);
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

            if let Some(old_ann_id) = gen_idx.unit_to_ann_id.insert(unit_id, ann_id)
                && let Some(entry) = gen_idx.metadata.get_mut(&old_ann_id)
            {
                entry.deleted = true;
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
            let index_guard = self.candidate_index.lock().map_err(|_| {
                SemanticError::StoreUnavailable("candidate index mutex poisoned".into())
            })?;
            let gen_idx = index_guard.get(&generation_id).ok_or_else(|| {
                SemanticError::StoreUnavailable(format!(
                    "candidate index for generation {generation_id} missing after sync"
                ))
            })?;
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

    // ── reconcile inputs and queue hygiene ─────────────────────────────────

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

    /// Drop any non-INFLIGHT queue row (occurrence_id == retrieval_unit_id by
    /// construction) for a unit that is no longer selected, so the queue only
    /// ever holds currently-selected work. INFLIGHT rows are never deleted: a
    /// concurrent worker owns them and completes or releases them itself.
    pub fn queue_retain_only(&self, keep: &[String]) -> Result<usize, SemanticError> {
        let conn = self.guard()?;
        if keep.is_empty() {
            return Ok(conn.execute("DELETE FROM sem_queue_v2 WHERE state != 'INFLIGHT'", [])?);
        }
        // A `NOT IN (?, ?, …)` list binds one variable per kept unit and fails
        // past SQLite's variable limit (32,766) — i.e. on any repo with more
        // than ~32K selected units. Stage the keep-set in a temp table instead.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS sem_keep_ids (id TEXT PRIMARY KEY);
             DELETE FROM temp.sem_keep_ids;",
        )?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO temp.sem_keep_ids (id) VALUES (?1)")?;
            for id in keep {
                ins.execute(params![id])?;
            }
        }
        let n = tx.execute(
            "DELETE FROM sem_queue_v2
              WHERE state != 'INFLIGHT'
                AND occurrence_id NOT IN (SELECT id FROM temp.sem_keep_ids)",
            [],
        )?;
        tx.execute("DELETE FROM temp.sem_keep_ids", [])?;
        tx.commit()?;
        Ok(n)
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
    /// `purge_inactive_models`, `purge_model`, and completed queue rows;
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
    use crate::provider::ExecutionBackend;

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

        for table in &[
            "sem_embeddings",
            "sem_query_demand",
            "sem_embeddings_v2",
            "sem_embedding_occurrences",
            "sem_queue_v2",
        ] {
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

    /// r02: one canonical embedding serves many occurrences; the same body
    /// re-put is a no-op, and every occurrence stays individually addressable.
    #[test]
    fn v2_canonical_embedding_shared_across_occurrences() {
        let s = SemanticStore::open_in_memory().unwrap();
        let fp = EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: "rev-q8".into(),
            dimension: 4,
            pooling_version: "last_token_v1".into(),
            normalization_version: "l2_unit_v1".into(),
            tokenizer_version: "tok_v1".into(),
            chunking_version: "2.0.0".into(),
            query_instruction_version: "code_retrieval_v1".into(),
            execution_backend: ExecutionBackend::OrtDirectMl,
            quantization: "test-none".to_string(),
        };
        let vsid = fp.vector_space_id();
        let cgid = fp.content_generation_id("sel_v1");

        let body = "{\"a\":1}";
        let hash = crate::identity::content_hash(body);
        assert!(
            s.put_canonical_embedding(&vsid, &hash, &[1.0, 0.0, 0.0, 0.0])
                .unwrap()
        );
        // Identical canonical body: second put must NOT insert again.
        assert!(
            !s.put_canonical_embedding(&vsid, &hash, &[1.0, 0.0, 0.0, 0.0])
                .unwrap()
        );

        for (i, env) in ["DEV", "PROD"].iter().enumerate() {
            s.add_occurrence(
                &format!("occ-{i}"),
                &format!("unit-{i}"),
                &vsid,
                &hash,
                "repo-a",
                "rev-1",
                "gen-1",
                &cgid,
                &format!("{{\"environment\": \"{env}\"}}"),
            )
            .unwrap();
        }
        assert_eq!(s.occurrence_count_for_canonical(&vsid, &hash).unwrap(), 2);
        let vec = s.embedding_for_canonical(&vsid, &hash).unwrap().unwrap();
        assert_eq!(vec, vec![1.0, 0.0, 0.0, 0.0]);
    }

    /// A failed attempt must go to the back of its priority band: the next
    /// claim takes healthy work first, not the item that just failed.
    #[test]
    fn v2_queue_failed_item_retries_behind_healthy_work() {
        let s = SemanticStore::open_in_memory().unwrap();
        let fp = EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "m".into(),
            model_revision: "r".into(),
            dimension: 2,
            pooling_version: "p".into(),
            normalization_version: "n".into(),
            tokenizer_version: "t".into(),
            chunking_version: "c".into(),
            query_instruction_version: "q".into(),
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "test-none".to_string(),
        };
        let vsid = fp.vector_space_id();
        let cgid = fp.content_generation_id("sel");
        for id in ["bad", "good"] {
            let hash = crate::identity::content_hash(id);
            s.put_canonical_embedding(&vsid, &hash, &[1.0, 0.0])
                .unwrap();
            s.add_occurrence(id, id, &vsid, &hash, "repo", "rev", "gen", &cgid, "{}")
                .unwrap();
        }
        s.queue_enqueue("bad", 0.5).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        s.queue_enqueue("good", 0.5).unwrap();

        let first = s.queue_claim_batch("w", 60_000, 1).unwrap();
        assert_eq!(first[0].0, "bad", "oldest first");
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(
            s.queue_mark_failed("bad", "w", first[0].1, 3, "boom")
                .unwrap()
        );

        let next = s.queue_claim_batch("w", 60_000, 2).unwrap();
        let order: Vec<&str> = next.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, vec!["good", "bad"]);
    }

    /// r02: lease + fencing — a reclaimed row's old owner must be unable to
    /// heartbeat or complete; the new owner's token is accepted.
    #[test]
    fn v2_queue_lease_fencing_rejects_stale_worker() {
        let s = SemanticStore::open_in_memory().unwrap();
        let fp = EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "m".into(),
            model_revision: "r".into(),
            dimension: 2,
            pooling_version: "p".into(),
            normalization_version: "n".into(),
            tokenizer_version: "t".into(),
            chunking_version: "c".into(),
            query_instruction_version: "q".into(),
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "test-none".to_string(),
        };
        let vsid = fp.vector_space_id();
        let cgid = fp.content_generation_id("sel");
        let hash = crate::identity::content_hash("body");
        s.put_canonical_embedding(&vsid, &hash, &[1.0, 0.0])
            .unwrap();
        s.add_occurrence(
            "occ-1", "unit-1", &vsid, &hash, "repo", "rev", "gen", &cgid, "{}",
        )
        .unwrap();
        s.queue_enqueue("occ-1", 0.5).unwrap();

        // Worker A claims with a 1 ms lease.
        let token_a = s.queue_claim("occ-1", "worker-a", 1).unwrap().unwrap();
        // While A's lease is valid, worker B cannot claim.
        assert!(
            s.queue_claim("occ-1", "worker-b", 60_000)
                .unwrap()
                .is_none()
        );
        // A can heartbeat with its token.
        assert!(s.queue_heartbeat("occ-1", "worker-a", token_a, 1).unwrap());

        // Lease expires (1 ms) → B reclaims with a NEW fencing token.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let reclaimed = s.queue_reclaim_expired().unwrap();
        assert_eq!(reclaimed, 1);
        let token_b = s.queue_claim("occ-1", "worker-b", 60_000).unwrap().unwrap();
        assert!(token_b > token_a, "reclaim must bump the fencing token");

        // Stale worker A: heartbeat and completion must both be rejected.
        assert!(!s.queue_heartbeat("occ-1", "worker-a", token_a, 1).unwrap());
        assert!(!s.queue_complete("occ-1", "worker-a", token_a).unwrap());

        // Current worker B completes with its token.
        assert!(s.queue_complete("occ-1", "worker-b", token_b).unwrap());
        assert_eq!(s.queue_counts().unwrap().done, 1);
    }

    /// r02: a stale fencing token must not be able to commit through
    /// `commit_batch` either — not just the narrower single-row
    /// `queue_complete`. Simulates "server death after inference but
    /// before commit": worker A claims, its lease expires (server died),
    /// worker B reclaims and completes first; A's late commit attempt with
    /// its old token must be silently dropped, not accepted and not an
    /// error, and must not create a duplicate canonical vector.
    #[test]
    fn commit_v2_batch_rejects_stale_fencing_token_and_creates_no_duplicate() {
        let s = SemanticStore::open_in_memory().unwrap();
        let fp = EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "m".into(),
            model_revision: "r".into(),
            dimension: 2,
            pooling_version: "p".into(),
            normalization_version: "n".into(),
            tokenizer_version: "t".into(),
            chunking_version: "c".into(),
            query_instruction_version: "q".into(),
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "test-none".to_string(),
        };
        let vsid = fp.vector_space_id();
        let cgid = fp.content_generation_id("sel");
        let hash = crate::identity::content_hash("body");
        s.add_occurrence(
            "occ-1", "unit-1", &vsid, &hash, "repo", "rev", "gen", &cgid, "{}",
        )
        .unwrap();
        s.queue_enqueue("occ-1", 0.5).unwrap();

        // Worker A claims with an already-expired lease (simulates a server
        // that died mid-inference: the lease clock ran out before it could
        // commit).
        let token_a = s.queue_claim("occ-1", "worker-a", -1).unwrap().unwrap();
        let reclaimed = s.queue_reclaim_expired().unwrap();
        assert_eq!(reclaimed, 1, "A's expired lease must be reclaimable");
        let token_b = s.queue_claim("occ-1", "worker-b", 60_000).unwrap().unwrap();
        assert!(token_b > token_a);

        // B completes normally.
        let entry_b = CommitEntry {
            occurrence_id: "occ-1".into(),
            owner: "worker-b".into(),
            fencing_token: token_b,
            retrieval_unit_id: "unit-1".into(),
            repository_id: "repo".into(),
            source_revision_id: "rev".into(),
            index_generation_id: "gen".into(),
            vector_space_id: vsid.clone(),
            canonical_hash: hash.clone(),
            provider_id: "qwen3".into(),
            model_id: "m".into(),
            vector: vec![1.0, 0.0],
        };
        let committed = s
            .commit_batch(std::slice::from_ref(&entry_b), 1, "sel-v1")
            .unwrap();
        assert_eq!(committed, vec!["occ-1".to_string()]);

        // A's late commit attempt with the stale token: must be dropped
        // silently (empty result, not an error) and must NOT overwrite the
        // canonical vector with a second one.
        let mut entry_a = entry_b.clone();
        entry_a.owner = "worker-a".into();
        entry_a.fencing_token = token_a;
        entry_a.vector = vec![9.0, 9.0]; // a divergent/late result
        let committed_stale = s.commit_batch(&[entry_a], 1, "sel-v1").unwrap();
        assert!(
            committed_stale.is_empty(),
            "stale-token commit must not be accepted"
        );

        let stored = s.embedding_for_canonical(&vsid, &hash).unwrap().unwrap();
        assert_eq!(
            stored,
            vec![1.0, 0.0],
            "no duplicate/overwritten canonical vector from the stale commit"
        );
        assert_eq!(
            s.queue_counts().unwrap().done,
            1,
            "exactly one completion, not two"
        );
    }

    #[test]
    fn guarded_repository_eviction_preserves_active_repo_vectors() {
        let s = SemanticStore::open_in_memory().unwrap();
        let configured_root = PathBuf::from(r"C:\workspace");
        let repo_root = configured_root.join("repo");
        let fp = EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "m".into(),
            model_revision: "r".into(),
            dimension: 2,
            pooling_version: "p".into(),
            normalization_version: "n".into(),
            tokenizer_version: "t".into(),
            chunking_version: "c".into(),
            query_instruction_version: "q".into(),
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "test-none".to_string(),
        };
        let vsid = fp.vector_space_id();
        let cgid = fp.content_generation_id("sel");
        let hash = crate::identity::content_hash("body");
        assert!(
            s.put_canonical_embedding(&vsid, &hash, &[1.0, 0.0])
                .unwrap()
        );
        s.add_occurrence(
            "occ-1", "unit-1", &vsid, &hash, "repo-a", "rev", "gen", &cgid, "{}",
        )
        .unwrap();
        s.queue_enqueue("occ-1", 0.5).unwrap();

        s.sync_workspace_membership(&[configured_root]).unwrap();
        let skipped = s
            .evict_repository_if_inactive("repo-a", &repo_root)
            .unwrap();
        assert!(
            skipped.is_none(),
            "an active root must cancel semantic eviction before deleting vectors"
        );
        assert!(
            s.embedding_for_canonical(&vsid, &hash).unwrap().is_some(),
            "active repository vectors must survive the cancelled eviction"
        );
        assert_eq!(s.queue_counts().unwrap().pending, 1);

        s.sync_workspace_membership(&[]).unwrap();
        let deleted = s
            .evict_repository_if_inactive("repo-a", &repo_root)
            .unwrap()
            .expect("inactive root must delete semantic rows");
        assert!(deleted > 0);
        assert!(
            s.embedding_for_canonical(&vsid, &hash).unwrap().is_none(),
            "inactive repository vectors must be removed"
        );
        assert_eq!(s.occurrence_count_for_canonical(&vsid, &hash).unwrap(), 0);
        assert_eq!(s.queue_counts().unwrap().pending, 0);
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
    fn database_from_another_schema_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE sem_schema_migrations (id TEXT PRIMARY KEY, applied_at INTEGER);
                 INSERT INTO sem_schema_migrations VALUES ('0001_initial', 0), ('0002_identity_leases', 0);
                 CREATE TABLE sem_queue (retrieval_unit_id TEXT PRIMARY KEY, state TEXT);
                 CREATE TABLE sem_vector_spaces (vector_space_id TEXT PRIMARY KEY);
                 CREATE TABLE sem_learned_tuning (tuning_key_hash TEXT PRIMARY KEY);",
            )
            .unwrap();
        }
        let s = SemanticStore::open(&path).unwrap();
        let conn = s.guard().unwrap();
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE name IN ('sem_queue', 'sem_vector_spaces', 'sem_learned_tuning')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old, 0, "tables from another schema must be gone");
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM sem_schema_migrations")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, vec!["0001_initial"]);
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
            execution_backend: ExecutionBackend::Unknown,
            quantization: "test-none".to_string(),
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
            execution_backend: ExecutionBackend::Unknown,
            quantization: "test-none".to_string(),
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
            "sem_query_demand",
            "sem_generations",
            "sem_embeddings_v2",
            "sem_embedding_occurrences",
            "sem_queue_v2",
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
            "idx_sem_gen_status",
            "idx_sem_occ_unit",
            "idx_sem_occ_canonical",
            "idx_sem_occ_generation",
            "idx_sem_queue_v2_state",
            "idx_sem_queue_v2_lease",
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

        // 3. Verify exactly the known baseline migrations are recorded
        let migration_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sem_schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(migration_count, 1, "only the baseline is recorded");

        let mut stmt = conn
            .prepare("SELECT id FROM sem_schema_migrations ORDER BY id")
            .unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(ids, vec!["0001_initial"]);
        for gone in ["sem_vector_spaces", "sem_content_generations", "sem_queue"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                    params![gone],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 0, "{gone} must not exist");
        }
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
