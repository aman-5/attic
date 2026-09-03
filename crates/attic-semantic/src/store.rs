//! Semantic store (Phase 5 §8): a SEPARATE, disposable SQLite database.
//!
//! Deliberate design decisions (ADR-014):
//! * Lives in its own file (`semantic.db`) next to the canonical index —
//!   deleting it must never affect canonical intelligence (tested).
//! * Canonical SQLite entities are NOT contaminated with provider-specific
//!   vector assumptions; this file can be dropped and rebuilt at any time.
//! * Nearest-neighbor search is a bounded brute-force scan over the ACTIVE
//!   model's rows with cached norms. At Phase 5 scales (≤ tens of thousands
//!   of SELECTED units) measured latency is sub-millisecond; an external
//!   vector database would add operational cost without measured need
//!   (value gate §24). Revisit only with benchmark evidence (OQ-023).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::error::SemanticError;
use crate::provider::CancelFlag;
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

/// Shared-handle-safe semantic store: rusqlite connections are `!Sync`, so
/// every access goes through an internal mutex (contention is negligible at
/// Phase 5 scales; queries hold it only for bounded reads).
#[derive(Debug)]
pub struct SemanticStore {
    conn: Mutex<Connection>,
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
        })
    }

    /// In-memory store for unit tests.
    pub fn open_in_memory() -> Result<Self, SemanticError> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
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

    fn migrate(conn: &Connection) -> Result<(), SemanticError> {
        // `semantic.db` is intentionally separate from canonical `attic.db`,
        // but its durable schema is still migration-owned.  Keeping the SQL
        // under migrations/ makes the complete persistent schema auditable
        // without contaminating the canonical database with semantic tables.
        conn.execute_batch(SEMANTIC_MIGRATION_0001)?;
        Ok(())
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
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
            "INSERT INTO sem_embeddings
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

    /// Delete every embedding for one unit (all models) or one exact record
    /// when `provider`/`model` are given.
    pub fn delete(
        &self,
        unit_id: &str,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<usize, SemanticError> {
        match (provider, model) {
            (Some(p), Some(m)) => {
                let n = self.guard()?.execute(
                    "DELETE FROM sem_embeddings
                      WHERE retrieval_unit_id=?1 AND provider_id=?2 AND model_id=?3",
                    params![unit_id, p, m],
                )?;
                Ok(n)
            }
            _ => {
                let n = self.guard()?.execute(
                    "DELETE FROM sem_embeddings WHERE retrieval_unit_id=?1",
                    params![unit_id],
                )?;
                Ok(n)
            }
        }
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

    /// Bounded brute-force kNN over ONE active (provider, model). Vectors are
    /// L2-normalized at write time so cosine similarity is the dot product.
    ///
    /// The scan honors [`ScanBudget`] DURING iteration: cancellation, a wall
    /// clock deadline, or the row cap stop the scan immediately and the
    /// partial result is returned with `truncated_by_budget = true` — the
    /// caller decides how to degrade (never an unbounded wait).
    pub fn knn(
        &self,
        query: &[f32],
        k: usize,
        provider: &str,
        model: &str,
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
        let mut stmt = conn.prepare(
            "SELECT retrieval_unit_id, norm, vector FROM sem_embeddings
              WHERE provider_id=?1 AND model_id=?2
                AND (?3 IS NULL OR repository_id=?3)",
        )?;
        let mut rows = stmt.query(params![provider, model, repository_filter])?;
        // Bounded top-k via a small sorted list (k is policy-capped).
        let mut top: Vec<(f32, String)> = Vec::with_capacity(k + 1);
        let mut scanned: u64 = 0;
        let mut truncated = false;
        while let Some(r) = rows.next()? {
            if budget.exhausted(scanned) {
                truncated = true;
                break;
            }
            scanned += 1;
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
        let hits: Vec<NearestHit> = top
            .into_iter()
            .map(|(sim, id)| NearestHit {
                retrieval_unit_id: id,
                similarity: sim,
            })
            .collect();
        Ok(KnnResult {
            hits,
            rows_scanned: scanned,
            truncated_by_budget: truncated,
        })
    }

    /// Delete ALL embeddings whose (provider, model) differ from the active
    /// pair — model-change invalidation without touching canonical data.
    pub fn purge_inactive_models(
        &self,
        active_provider: &str,
        active_model: &str,
    ) -> Result<usize, SemanticError> {
        Ok(self.guard()?.execute(
            "DELETE FROM sem_embeddings WHERE provider_id!=?1 OR model_id!=?2",
            params![active_provider, active_model],
        )?)
    }

    /// Delete everything for one model (full semantic-layer reset).
    pub fn purge_model(&self, provider: &str, model: &str) -> Result<usize, SemanticError> {
        Ok(self.guard()?.execute(
            "DELETE FROM sem_embeddings WHERE provider_id=?1 AND model_id=?2",
            params![provider, model],
        )?)
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

    /// Take up to `limit` PENDING items (priority DESC, FIFO within equal
    /// priority) and mark them INFLIGHT.
    pub fn queue_take_batch(&self, limit: usize) -> Result<Vec<QueueItem>, SemanticError> {
        // Read phase: guard scoped so it is DEFINITELY dropped before the
        // write phase below (std Mutex is not reentrant — holding it across
        // the update loop would self-deadlock).
        let items: Vec<QueueItem> = {
            let conn = self.guard()?;
            let mut stmt = conn.prepare(
                "SELECT retrieval_unit_id, priority, attempts FROM sem_queue
                  WHERE state=?1
                  ORDER BY priority DESC, enqueued_at_ms ASC, retrieval_unit_id ASC
                  LIMIT ?2",
            )?;
            let mut out = Vec::new();
            let mut rows = stmt.query(params![Q_PENDING, limit as i64])?;
            while let Some(r) = rows.next()? {
                out.push(QueueItem {
                    retrieval_unit_id: r.get(0)?,
                    priority: r.get(1)?,
                    attempts: r.get::<_, i64>(2)? as u32,
                });
            }
            out
        };
        // Write phase: guard reacquired per statement.
        for it in &items {
            self.guard()?.execute(
                "UPDATE sem_queue SET state=?2 WHERE retrieval_unit_id=?1",
                params![it.retrieval_unit_id, Q_INFLIGHT],
            )?;
        }
        Ok(items)
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
    pub fn queue_retain_only(&self, keep: &[String]) -> Result<usize, SemanticError> {
        use rusqlite::ToSql;
        let n = if keep.is_empty() {
            self.conn
                .lock()
                .expect("semantic store mutex")
                .execute("DELETE FROM sem_queue", [])?
        } else {
            let paramslice: Vec<&dyn ToSql> = keep.iter().map(|s| s as &dyn ToSql).collect();
            let placeholders = vec!["?"; keep.len()].join(",");
            let sql =
                format!("DELETE FROM sem_queue WHERE retrieval_unit_id NOT IN ({placeholders})");
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
    fn knn_orders_by_similarity_and_filters_model() {
        let s = SemanticStore::open_in_memory().unwrap();
        s.put(&rec("near", vec![1.0, 0.0])).unwrap();
        s.put(&rec("far", vec![0.0, 1.0])).unwrap();
        let mut other = rec("other-model", vec![1.0, 0.0]);
        other.model_id = "old-model".into();
        s.put(&other).unwrap();

        let cancel = crate::provider::CancelFlag::new();
        let hits = s
            .knn(
                &[1.0, 0.0],
                2,
                "hashing",
                "hashed-ngram-v1",
                None,
                &ScanBudget::unbounded(&cancel),
            )
            .unwrap();
        assert_eq!(hits.hits.len(), 2);
        assert!(!hits.truncated_by_budget);
        assert_eq!(hits.hits[0].retrieval_unit_id, "near");
        assert!((hits.hits[0].similarity - 1.0).abs() < 1e-6);

        // Old-model row invisible under the active pair.
        let hits_old = s
            .knn(
                &[1.0, 0.0],
                10,
                "hashing",
                "old-model",
                None,
                &ScanBudget::unbounded(&cancel),
            )
            .unwrap();
        assert_eq!(hits_old.hits.len(), 1);
        assert_eq!(hits_old.hits[0].retrieval_unit_id, "other-model");
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
}
