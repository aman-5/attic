//! Learned Tuning Persistence and Invalidation (Final Master Plan V2 §25, CP17).
//!
//! Stores hardware/model/runtime-keyed optimal execution parameters:
//! - Keys: CPU architecture, OS, model ID, pinned model revision, dimension, runtime version.
//! - Values: recommended inference lane count, recommended batch size, granted CPU threads, observed chunks/sec.
//! - Invalidation: automatically invalidates cached tuning when any component of the tuning key changes,
//!   or on explicit reset.

use crate::error::SemanticError;
use rusqlite::{Connection, params};

/// Multi-attribute key identifying the exact execution context (hardware, model, runtime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuningKey {
    pub cpu_architecture: String,
    pub os_name: String,
    pub model_id: String,
    pub model_revision: String,
    pub dimension: usize,
    pub runtime_version: String,
}

impl TuningKey {
    /// Capture current machine environment with specified model and dimension.
    pub fn current(
        model_id: impl Into<String>,
        model_revision: impl Into<String>,
        dimension: usize,
    ) -> Self {
        Self {
            cpu_architecture: std::env::consts::ARCH.to_string(),
            os_name: std::env::consts::OS.to_string(),
            model_id: model_id.into(),
            model_revision: model_revision.into(),
            dimension,
            runtime_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Compute deterministic BLAKE3 hash for the tuning key.
    pub fn compute_key_hash(&self) -> String {
        let raw = format!(
            "{}|{}|{}|{}|{}|{}",
            self.cpu_architecture,
            self.os_name,
            self.model_id,
            self.model_revision,
            self.dimension,
            self.runtime_version
        );
        blake3::hash(raw.as_bytes()).to_hex().to_string()
    }
}

/// Stored record representing verified optimal resource allocation and throughput.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedTuningRecord {
    pub key: TuningKey,
    pub key_hash: String,
    pub recommended_lanes: usize,
    pub recommended_batch_size: usize,
    pub recommended_cpu_threads: usize,
    pub observed_chunks_per_sec: f64,
    pub updated_at_ms: i64,
}

impl LearnedTuningRecord {
    /// Create a new tuning record, automatically computing its key hash.
    pub fn new(
        key: TuningKey,
        recommended_lanes: usize,
        recommended_batch_size: usize,
        recommended_cpu_threads: usize,
        observed_chunks_per_sec: f64,
    ) -> Self {
        let key_hash = key.compute_key_hash();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            key,
            key_hash,
            recommended_lanes,
            recommended_batch_size,
            recommended_cpu_threads,
            observed_chunks_per_sec,
            updated_at_ms: now_ms,
        }
    }

    /// Verify whether this record remains valid for the target execution key.
    pub fn is_valid_for(&self, target_key: &TuningKey) -> bool {
        self.key == *target_key && self.key_hash == target_key.compute_key_hash()
    }
}

/// Manager for persisting and retrieving learned tuning from the semantic database.
#[derive(Debug, Default, Clone, Copy)]
pub struct LearnedTuningManager;

impl LearnedTuningManager {
    /// Read learned tuning for the specified execution key. Returns `None` if absent or invalidated.
    pub fn read_tuning(
        &self,
        conn: &Connection,
        key: &TuningKey,
    ) -> Result<Option<LearnedTuningRecord>, SemanticError> {
        let key_hash = key.compute_key_hash();
        let mut stmt = conn.prepare(
            "SELECT cpu_architecture, os_name, model_id, model_revision, dimension,
                    runtime_version, recommended_lanes, recommended_batch_size,
                    recommended_cpu_threads, observed_chunks_per_sec, updated_at_ms
             FROM sem_learned_tuning
             WHERE tuning_key_hash = ?1",
        )?;

        let mut rows = stmt.query(params![key_hash])?;
        if let Some(row) = rows.next()? {
            let cpu_arch: String = row.get(0)?;
            let os_name: String = row.get(1)?;
            let model_id: String = row.get(2)?;
            let model_revision: String = row.get(3)?;
            let dimension: usize = row.get::<_, i64>(4)? as usize;
            let runtime_ver: String = row.get(5)?;
            let lanes: usize = row.get::<_, i64>(6)? as usize;
            let batch: usize = row.get::<_, i64>(7)? as usize;
            let cpu_threads: usize = row.get::<_, i64>(8)? as usize;
            let chunks_per_sec: f64 = row.get(9)?;
            let updated_ms: i64 = row.get(10)?;

            let stored_key = TuningKey {
                cpu_architecture: cpu_arch,
                os_name,
                model_id,
                model_revision,
                dimension,
                runtime_version: runtime_ver,
            };

            // Invalidation invariant (§25): if stored key does not match query key, ignore
            if stored_key != *key {
                return Ok(None);
            }

            Ok(Some(LearnedTuningRecord {
                key: stored_key,
                key_hash,
                recommended_lanes: lanes,
                recommended_batch_size: batch,
                recommended_cpu_threads: cpu_threads,
                observed_chunks_per_sec: chunks_per_sec,
                updated_at_ms: updated_ms,
            }))
        } else {
            Ok(None)
        }
    }

    /// Save or replace learned tuning in the database.
    pub fn save_tuning(
        &self,
        conn: &Connection,
        record: &LearnedTuningRecord,
    ) -> Result<(), SemanticError> {
        conn.execute(
            "INSERT INTO sem_learned_tuning
                 (tuning_key_hash, cpu_architecture, os_name, model_id, model_revision,
                  dimension, runtime_version, recommended_lanes, recommended_batch_size,
                  recommended_cpu_threads, observed_chunks_per_sec, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(tuning_key_hash) DO UPDATE SET
                 recommended_lanes = excluded.recommended_lanes,
                 recommended_batch_size = excluded.recommended_batch_size,
                 recommended_cpu_threads = excluded.recommended_cpu_threads,
                 observed_chunks_per_sec = excluded.observed_chunks_per_sec,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                record.key_hash,
                record.key.cpu_architecture,
                record.key.os_name,
                record.key.model_id,
                record.key.model_revision,
                record.key.dimension as i64,
                record.key.runtime_version,
                record.recommended_lanes as i64,
                record.recommended_batch_size as i64,
                record.recommended_cpu_threads as i64,
                record.observed_chunks_per_sec,
                record.updated_at_ms,
            ],
        )?;
        Ok(())
    }

    /// Invalidate/remove tuning for a specific key.
    pub fn invalidate_tuning(
        &self,
        conn: &Connection,
        key: &TuningKey,
    ) -> Result<bool, SemanticError> {
        let key_hash = key.compute_key_hash();
        let affected = conn.execute(
            "DELETE FROM sem_learned_tuning WHERE tuning_key_hash = ?1",
            params![key_hash],
        )?;
        Ok(affected > 0)
    }

    /// Purge all stored tuning (e.g. on complete re-index or manual reset).
    pub fn purge_all(&self, conn: &Connection) -> Result<usize, SemanticError> {
        let affected = conn.execute("DELETE FROM sem_learned_tuning", [])?;
        Ok(affected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!(
            "../../../migrations/semantic/0001_initial.sql"
        ))
        .unwrap();
        conn
    }

    #[test]
    fn tuning_key_hash_is_sensitive_to_every_attribute() {
        let base = TuningKey {
            cpu_architecture: "x86_64".into(),
            os_name: "windows".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: "rev_1".into(),
            dimension: 1024,
            runtime_version: "0.1.0".into(),
        };
        let hash_base = base.compute_key_hash();

        // Altering CPU architecture changes hash
        let mut altered = base.clone();
        altered.cpu_architecture = "aarch64".into();
        assert_ne!(hash_base, altered.compute_key_hash());

        // Altering OS changes hash
        let mut altered = base.clone();
        altered.os_name = "linux".into();
        assert_ne!(hash_base, altered.compute_key_hash());

        // Altering model revision changes hash
        let mut altered = base.clone();
        altered.model_revision = "rev_2".into();
        assert_ne!(hash_base, altered.compute_key_hash());

        // Altering dimension changes hash
        let mut altered = base.clone();
        altered.dimension = 768;
        assert_ne!(hash_base, altered.compute_key_hash());

        // Altering runtime version changes hash
        let mut altered = base.clone();
        altered.runtime_version = "0.2.0".into();
        assert_ne!(hash_base, altered.compute_key_hash());
    }

    #[test]
    fn save_read_and_invalidate_lifecycle() {
        let conn = in_memory_db();
        let mgr = LearnedTuningManager;

        let key = TuningKey {
            cpu_architecture: "x86_64".into(),
            os_name: "windows".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: "c42b6a9".into(),
            dimension: 1024,
            runtime_version: "0.1.0".into(),
        };

        // Initially absent
        let absent = mgr.read_tuning(&conn, &key).unwrap();
        assert!(absent.is_none());

        // Save tuning
        let rec = LearnedTuningRecord::new(key.clone(), 2, 32, 4, 185.5);
        mgr.save_tuning(&conn, &rec).unwrap();

        // Read back
        let read = mgr
            .read_tuning(&conn, &key)
            .unwrap()
            .expect("should find saved record");
        assert_eq!(read.recommended_lanes, 2);
        assert_eq!(read.recommended_batch_size, 32);
        assert_eq!(read.recommended_cpu_threads, 4);
        assert!((read.observed_chunks_per_sec - 185.5).abs() < 1e-4);
        assert!(read.is_valid_for(&key));

        // Different revision results in cache miss (automatic invalidation)
        let other_key = TuningKey {
            model_revision: "different_hash".into(),
            ..key.clone()
        };
        assert!(mgr.read_tuning(&conn, &other_key).unwrap().is_none());

        // Explicit invalidation
        let deleted = mgr.invalidate_tuning(&conn, &key).unwrap();
        assert!(deleted);
        assert!(mgr.read_tuning(&conn, &key).unwrap().is_none());
    }
}
