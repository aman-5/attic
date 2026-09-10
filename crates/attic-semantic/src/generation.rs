//! Semantic generations, vector space isolation, and rollback management (Master Plan V2 §51–§55, CP10).
//!
//! Enforces:
//! - Complete vector space isolation: queries only search the ACTIVE generation.
//! - Non-blocking rebuilds: new generations are built in the background while the active generation continues serving queries (§54).
//! - Atomic generation activation: switching from old to new generation happens in a single transaction (§52, §53).
//! - Safe rollback: previous complete generations are retained and can be reactivated instantly on failure (§55).

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::SemanticError;
use crate::provider::EmbeddingFingerprint;

/// Operational status of a semantic generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationStatus {
    /// Vectors are actively being computed and inserted. Queries do not read this generation.
    Building,
    /// Active generation serving all search queries.
    Active,
    /// Previously active generation retained for rollback.
    Superseded,
    /// Generation that was rolled back due to failure or regression.
    RolledBack,
    /// Generation marked for deletion.
    Discarded,
}

impl GenerationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Building => "BUILDING",
            Self::Active => "ACTIVE",
            Self::Superseded => "SUPERSEDED",
            Self::RolledBack => "ROLLEDBACK",
            Self::Discarded => "DISCARDED",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "ACTIVE" => Self::Active,
            "SUPERSEDED" => Self::Superseded,
            "ROLLEDBACK" => Self::RolledBack,
            "DISCARDED" => Self::Discarded,
            _ => Self::Building,
        }
    }
}

/// Metadata describing a specific semantic generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation_id: i64,
    pub fingerprint: EmbeddingFingerprint,
    pub status: GenerationStatus,
    pub unit_count: u64,
    pub created_at_ms: i64,
    pub activated_at_ms: Option<i64>,
}

/// Controller managing semantic generations, promotions, and rollbacks.
pub struct GenerationManager;

impl GenerationManager {
    /// Retrieve the currently ACTIVE generation, if any.
    pub fn get_active_generation(
        conn: &Connection,
    ) -> Result<Option<GenerationRecord>, SemanticError> {
        let res = conn
            .query_row(
                "SELECT generation_id, fingerprint_json, status, unit_count, created_at_ms, activated_at_ms \
                 FROM sem_generations WHERE status = 'ACTIVE' ORDER BY generation_id DESC LIMIT 1",
                [],
                |row| {
                    let id: i64 = row.get(0)?;
                    let fp_json: String = row.get(1)?;
                    let status_str: String = row.get(2)?;
                    let unit_count = row.get::<_, i64>(3)? as u64;
                    let created_at: i64 = row.get(4)?;
                    let activated_at: Option<i64> = row.get(5)?;
                    Ok((id, fp_json, status_str, unit_count, created_at, activated_at))
                },
            )
            .optional()?;

        match res {
            Some((id, fp_json, status_str, unit_count, created_at, activated_at)) => {
                let fingerprint: EmbeddingFingerprint =
                    serde_json::from_str(&fp_json).map_err(|e| {
                        SemanticError::StoreUnavailable(format!(
                            "corrupt fingerprint JSON in generation: {e}"
                        ))
                    })?;
                Ok(Some(GenerationRecord {
                    generation_id: id,
                    fingerprint,
                    status: GenerationStatus::from_str(&status_str),
                    unit_count,
                    created_at_ms: created_at,
                    activated_at_ms: activated_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// Retrieve the currently BUILDING generation, if any.
    pub fn get_building_generation(
        conn: &Connection,
    ) -> Result<Option<GenerationRecord>, SemanticError> {
        let res = conn
            .query_row(
                "SELECT generation_id, fingerprint_json, status, unit_count, created_at_ms, activated_at_ms \
                 FROM sem_generations WHERE status = 'BUILDING' ORDER BY generation_id DESC LIMIT 1",
                [],
                |row| {
                    let id: i64 = row.get(0)?;
                    let fp_json: String = row.get(1)?;
                    let status_str: String = row.get(2)?;
                    let unit_count = row.get::<_, i64>(3)? as u64;
                    let created_at: i64 = row.get(4)?;
                    let activated_at: Option<i64> = row.get(5)?;
                    Ok((id, fp_json, status_str, unit_count, created_at, activated_at))
                },
            )
            .optional()?;

        match res {
            Some((id, fp_json, status_str, unit_count, created_at, activated_at)) => {
                let fingerprint: EmbeddingFingerprint =
                    serde_json::from_str(&fp_json).map_err(|e| {
                        SemanticError::StoreUnavailable(format!(
                            "corrupt fingerprint JSON in generation: {e}"
                        ))
                    })?;
                Ok(Some(GenerationRecord {
                    generation_id: id,
                    fingerprint,
                    status: GenerationStatus::from_str(&status_str),
                    unit_count,
                    created_at_ms: created_at,
                    activated_at_ms: activated_at,
                }))
            }
            None => Ok(None),
        }
    }

    /// Start a new generation in `Building` state.
    pub fn start_new_generation(
        conn: &Connection,
        fingerprint: &EmbeddingFingerprint,
    ) -> Result<GenerationRecord, SemanticError> {
        let fp_json = serde_json::to_string(fingerprint).map_err(|e| {
            SemanticError::StoreUnavailable(format!("failed to serialize fingerprint: {e}"))
        })?;
        let fp_hash = blake3::hash(fp_json.as_bytes()).to_hex().to_string();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        conn.execute(
            "INSERT INTO sem_generations (fingerprint_json, fingerprint_hash, status, unit_count, created_at_ms) \
             VALUES (?1, ?2, 'BUILDING', 0, ?3)",
            params![fp_json, fp_hash, now_ms],
        )?;

        let generation_id = conn.last_insert_rowid();

        Ok(GenerationRecord {
            generation_id,
            fingerprint: fingerprint.clone(),
            status: GenerationStatus::Building,
            unit_count: 0,
            created_at_ms: now_ms,
            activated_at_ms: None,
        })
    }

    /// Atomically activate a generation, superseding any currently active generation.
    pub fn activate_generation(
        conn: &mut Connection,
        target_generation_id: i64,
    ) -> Result<(), SemanticError> {
        let tx = conn.transaction()?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Transition existing ACTIVE generation to SUPERSEDED
        tx.execute(
            "UPDATE sem_generations SET status = 'SUPERSEDED' WHERE status = 'ACTIVE'",
            [],
        )?;

        // Transition target generation to ACTIVE
        let rows = tx.execute(
            "UPDATE sem_generations SET status = 'ACTIVE', activated_at_ms = ?1 WHERE generation_id = ?2",
            params![now_ms, target_generation_id],
        )?;

        if rows == 0 {
            return Err(SemanticError::StoreUnavailable(format!(
                "generation {} does not exist",
                target_generation_id
            )));
        }

        tx.commit()?;
        Ok(())
    }

    /// Roll back to the most recent superseded complete generation (§55).
    pub fn rollback_to_previous(
        conn: &mut Connection,
    ) -> Result<Option<GenerationRecord>, SemanticError> {
        let tx = conn.transaction()?;

        // Find the latest SUPERSEDED generation
        let prev_gen_id: Option<i64> = tx
            .query_row(
                "SELECT generation_id FROM sem_generations WHERE status = 'SUPERSEDED' ORDER BY generation_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;

        let Some(target_id) = prev_gen_id else {
            return Ok(None); // No previous generation available for rollback
        };

        // Mark currently active generation as ROLLEDBACK
        tx.execute(
            "UPDATE sem_generations SET status = 'ROLLEDBACK' WHERE status = 'ACTIVE'",
            [],
        )?;

        // Reactivate target generation
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        tx.execute(
            "UPDATE sem_generations SET status = 'ACTIVE', activated_at_ms = ?1 WHERE generation_id = ?2",
            params![now_ms, target_id],
        )?;

        tx.commit()?;

        Self::get_active_generation(conn)
    }

    /// Prune old superseded and rolled back generations according to bounded retention policy.
    pub fn prune_old_generations(
        conn: &mut Connection,
        keep_max: usize,
    ) -> Result<usize, SemanticError> {
        let tx = conn.transaction()?;

        // Find IDs of superseded/rolled-back generations beyond keep_max
        let mut stmt = tx.prepare(
            "SELECT generation_id FROM sem_generations \
             WHERE status IN ('SUPERSEDED', 'ROLLEDBACK') \
             ORDER BY generation_id DESC LIMIT -1 OFFSET ?1",
        )?;
        let rows = stmt.query_map(params![keep_max as i64], |r| r.get::<_, i64>(0))?;
        let to_prune: Vec<i64> = rows.filter_map(Result::ok).collect();
        drop(stmt);

        let count = to_prune.len();
        for gen_id in to_prune {
            // Delete vectors belonging to this generation
            tx.execute(
                "DELETE FROM sem_embeddings WHERE generation_id = ?1",
                params![gen_id],
            )?;
            // Mark or delete generation row
            tx.execute(
                "DELETE FROM sem_generations WHERE generation_id = ?1",
                params![gen_id],
            )?;
        }

        tx.commit()?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_fingerprint(model: &str) -> EmbeddingFingerprint {
        EmbeddingFingerprint {
            provider: "qwen3".to_string(),
            model_id: model.to_string(),
            model_revision: "rev1".to_string(),
            dimension: 768,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "qwen3_tok_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_retrieval_v1".to_string(),
        }
    }

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sem_generations (
                generation_id    INTEGER PRIMARY KEY AUTOINCREMENT,
                fingerprint_json TEXT NOT NULL,
                fingerprint_hash TEXT NOT NULL,
                status           TEXT NOT NULL,
                unit_count       INTEGER NOT NULL DEFAULT 0,
                created_at_ms    INTEGER NOT NULL,
                activated_at_ms  INTEGER
            );
            CREATE TABLE sem_embeddings (
                retrieval_unit_id TEXT NOT NULL,
                generation_id    INTEGER NOT NULL,
                vector           BLOB NOT NULL,
                PRIMARY KEY (retrieval_unit_id, generation_id)
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn start_and_activate_generation() {
        let mut conn = setup_test_db();
        let fp1 = test_fingerprint("qwen3-768");
        let gen1 = GenerationManager::start_new_generation(&conn, &fp1).unwrap();
        assert_eq!(gen1.generation_id, 1);
        assert_eq!(gen1.status, GenerationStatus::Building);

        // Activate Gen 1
        GenerationManager::activate_generation(&mut conn, 1).unwrap();
        let active = GenerationManager::get_active_generation(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(active.generation_id, 1);
        assert_eq!(active.status, GenerationStatus::Active);

        // Start Gen 2 (Qwen 1024-dim)
        let fp2 = test_fingerprint("qwen3-1024");
        let gen2 = GenerationManager::start_new_generation(&conn, &fp2).unwrap();
        assert_eq!(gen2.generation_id, 2);
        assert_eq!(gen2.status, GenerationStatus::Building);

        // While Gen 2 is building, active is still Gen 1
        let active_now = GenerationManager::get_active_generation(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(active_now.generation_id, 1);

        // Activate Gen 2
        GenerationManager::activate_generation(&mut conn, 2).unwrap();
        let active_final = GenerationManager::get_active_generation(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(active_final.generation_id, 2);
    }

    #[test]
    fn rollback_reactivates_previous_generation() {
        let mut conn = setup_test_db();
        let fp1 = test_fingerprint("qwen3-768");
        let _ = GenerationManager::start_new_generation(&conn, &fp1).unwrap();
        GenerationManager::activate_generation(&mut conn, 1).unwrap();

        let fp2 = test_fingerprint("qwen3-1024");
        let _ = GenerationManager::start_new_generation(&conn, &fp2).unwrap();
        GenerationManager::activate_generation(&mut conn, 2).unwrap();

        // Roll back: should deactivate Gen 2 and reactivate Gen 1
        let restored = GenerationManager::rollback_to_previous(&mut conn)
            .unwrap()
            .unwrap();
        assert_eq!(restored.generation_id, 1);
        assert_eq!(restored.status, GenerationStatus::Active);

        let active = GenerationManager::get_active_generation(&conn)
            .unwrap()
            .unwrap();
        assert_eq!(active.generation_id, 1);
    }
}
