//! Phase 2 — invalidation DAG primitives (invalidation contract C08).
//!
//! Every function here is a **transaction-assuming primitive** safe to call
//! inside a writer-queue closure; invalidation state flags and
//! `core_invalidation_records` rows are written atomically in the same
//! transaction (contract invariant 5).
//!
//! Propagation implemented per contract §Propagation Rules:
//! FileOccurrence → StructuralNode → SymbolOccurrence / Relationship,
//! FileOccurrence → RetrievalUnit (→ SemanticRepr, future),
//! FileOccurrence → KnowledgeItem, and Evidence marked STALE.

use rusqlite::Connection;
use serde::Serialize;

use attic_core::{FreshnessState, InvalidationArtifactType, InvalidationCause};

use crate::error::StorageError;

/// Counts produced by one propagation run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct InvalidationCounts {
    /// Occurrence rows whose freshness was rewritten.
    pub occurrences: u64,
    /// Structural nodes invalidated.
    pub structural_nodes: u64,
    /// Symbol occurrences invalidated.
    pub symbol_occurrences: u64,
    /// Relationships invalidated.
    pub relationships: u64,
    /// Retrieval units invalidated.
    pub retrieval_units: u64,
    /// Evidence records marked STALE.
    pub evidence_stale: u64,
    /// Knowledge items invalidated.
    pub knowledge_items: u64,
}

impl InvalidationCounts {
    /// Total number of artifacts touched (occurrence + derived).
    pub fn total(&self) -> u64 {
        self.occurrences
            + self.structural_nodes
            + self.symbol_occurrences
            + self.relationships
            + self.retrieval_units
            + self.evidence_stale
            + self.knowledge_items
    }
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

fn update_freshness(
    conn: &Connection,
    table: &str,
    id_column: &str,
    ids: &[String],
    state: FreshnessState,
) -> Result<u64, StorageError> {
    let mut updated = 0u64;
    for id in ids {
        let sql = format!("UPDATE {table} SET freshness_state = '{state}' WHERE {id_column} = ?1");
        updated += conn.execute(&sql, rusqlite::params![id])? as u64;
    }
    Ok(updated)
}

/// Append one `core_invalidation_records` row for every artifact id listed.
pub fn record_invalidation(
    conn: &Connection,
    artifact_type: InvalidationArtifactType,
    artifact_ids: &[String],
    cause: InvalidationCause,
    now_us: i64,
) -> Result<(), StorageError> {
    for artifact_id in artifact_ids {
        conn.execute(
            "INSERT INTO core_invalidation_records
                 (id, artifact_type, artifact_id, reason, invalidated_at, recomputed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                artifact_type.as_str(),
                artifact_id,
                cause.as_str(),
                now_us
            ],
        )?;
    }
    Ok(())
}

/// Mark pending invalidation records as recomputed for one artifact.
pub fn record_recomputed(
    conn: &Connection,
    artifact_id: &str,
    now_us: i64,
) -> Result<u64, StorageError> {
    let n = conn.execute(
        "UPDATE core_invalidation_records
            SET recomputed_at = ?2
          WHERE artifact_id = ?1 AND recomputed_at IS NULL",
        rusqlite::params![artifact_id, now_us],
    )?;
    Ok(n as u64)
}

/// Close every still-pending invalidation record belonging to a file
/// occurrence **and** its derived artifacts (units, nodes, symbols).
///
/// Called after a successful incremental republication replaces all derived
/// state of an occurrence; prevents permanently-pending audit rows from
/// polluting "pending work" reporting.  Idempotent.
pub fn close_pending_records_for_occurrence(
    conn: &Connection,
    occurrence_id: &str,
    now_us: i64,
) -> Result<u64, StorageError> {
    let n = conn.execute(
        "UPDATE core_invalidation_records
            SET recomputed_at = ?2
          WHERE recomputed_at IS NULL
            AND artifact_id IN (
                SELECT ?1
                UNION
                SELECT id FROM core_retrieval_units     WHERE file_occurrence_id = ?1
                UNION
                SELECT id FROM core_structural_nodes    WHERE file_occurrence_id = ?1
                UNION
                SELECT id FROM core_symbol_occurrences  WHERE file_occurrence_id = ?1
                UNION
                SELECT r.id FROM core_relationships r
                 WHERE r.source_entity_id IN (
                       SELECT id FROM core_symbol_occurrences WHERE file_occurrence_id = ?1)
                    OR r.target_entity_id IN (
                       SELECT id FROM core_symbol_occurrences WHERE file_occurrence_id = ?1)
                    OR r.source_entity_id = ?1
                    OR (r.target_entity_type = 'FILE_OCCURRENCE'
                        AND r.target_entity_id = ?1)
            )",
        rusqlite::params![occurrence_id, now_us],
    )?;
    Ok(n as u64)
}

// ---------------------------------------------------------------------------
// Propagation
// ---------------------------------------------------------------------------

/// Invalidate all artifacts derived from the given file occurrences and set
/// the occurrences themselves to `occurrence_state`.
///
/// Per contract:
/// - structural nodes / symbol occurrences / retrieval units / knowledge items
///   → `INVALID`
/// - evidence → `STALE` (kept, with staleness metadata — INV-Q2)
/// - one `core_invalidation_records` row per touched artifact
///
/// Derived artifacts inherit `InvalidationCause`; the occurrence itself is
/// recorded with the caller's cause.  Semantic representations do not exist
/// before Phase 5 and are intentionally not touched.
pub fn invalidate_for_occurrences(
    conn: &Connection,
    occurrence_ids: &[String],
    occurrence_state: FreshnessState,
    cause: InvalidationCause,
    now_us: i64,
) -> Result<InvalidationCounts, StorageError> {
    let mut counts = InvalidationCounts::default();
    if occurrence_ids.is_empty() {
        return Ok(counts);
    }

    counts.occurrences = update_freshness(
        conn,
        "core_file_occurrences",
        "id",
        occurrence_ids,
        occurrence_state,
    )?;
    record_invalidation(
        conn,
        InvalidationArtifactType::FileOccurrence,
        occurrence_ids,
        cause,
        now_us,
    )?;

    // Structural nodes depend on the file occurrence.
    let mut structural_ids: Vec<String> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT id FROM core_structural_nodes WHERE file_occurrence_id = ?1")?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                structural_ids.push(row?);
            }
        }
    }
    if !structural_ids.is_empty() {
        counts.structural_nodes = update_freshness(
            conn,
            "core_structural_nodes",
            "id",
            &structural_ids,
            FreshnessState::Invalid,
        )?;
        record_invalidation(
            conn,
            InvalidationArtifactType::StructuralNode,
            &structural_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    // Symbol occurrences depend on the file occurrence (and on nodes).
    let mut symbol_ids: Vec<String> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT id FROM core_symbol_occurrences WHERE file_occurrence_id = ?1")?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                symbol_ids.push(row?);
            }
        }
    }
    if !symbol_ids.is_empty() {
        // Unlike the other derived-artifact tables, `core_symbol_occurrences`
        // has no `freshness_state` column of its own (see
        // `migrations/0001_initial.sql`): its liveness is derived
        // transitively through `file_occurrence_id` -> the owning
        // `core_file_occurrences.freshness_state`, which was already
        // invalidated above. Every read path (`retrieval_reads.rs`,
        // `semantic_reads.rs`) filters on `fo.freshness_state`, never on a
        // per-row state here, and `repository/structural.rs` deletes rows
        // outright rather than flagging them. So there is nothing to update
        // on this table — only the audit trail is recorded.
        counts.symbol_occurrences = symbol_ids.len() as u64;
        record_invalidation(
            conn,
            InvalidationArtifactType::SymbolOccurrence,
            &symbol_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    // Relationships anchored at this file's occurrences (source side).  The
    // schema stores source/target entity ids as text references into
    // core_symbol_occurrences/core_file_occurrences; Phase 1D does not yet
    // populate relationships, so this is a cheap no-op today but keeps the
    // DAG complete for Phase 3+.
    let mut relationship_ids: Vec<String> = Vec::new();
    {
        // Edges anchored at symbol occurrences of this file AND edges
        // anchored at the file occurrence itself (imports, heritage emitted
        // at file scope by Phase 3 analyzers).
        let mut stmt = conn.prepare(
            "SELECT r.id
               FROM core_relationships r
              WHERE r.source_entity_id IN (
                    SELECT id FROM core_symbol_occurrences WHERE file_occurrence_id = ?1)
                 OR r.target_entity_id IN (
                    SELECT id FROM core_symbol_occurrences WHERE file_occurrence_id = ?1)
                 OR r.source_entity_id = ?1
                 OR (r.target_entity_type = 'FILE_OCCURRENCE' AND r.target_entity_id = ?1)",
        )?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                relationship_ids.push(row?);
            }
        }
    }
    if !relationship_ids.is_empty() {
        counts.relationships = update_freshness(
            conn,
            "core_relationships",
            "id",
            &relationship_ids,
            FreshnessState::Invalid,
        )?;
        record_invalidation(
            conn,
            InvalidationArtifactType::Relationship,
            &relationship_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    // Retrieval units depend on the file occurrence.
    let mut unit_ids: Vec<String> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT id FROM core_retrieval_units WHERE file_occurrence_id = ?1")?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                unit_ids.push(row?);
            }
        }
    }
    if !unit_ids.is_empty() {
        counts.retrieval_units = update_freshness(
            conn,
            "core_retrieval_units",
            "id",
            &unit_ids,
            FreshnessState::Invalid,
        )?;
        record_invalidation(
            conn,
            InvalidationArtifactType::RetrievalUnit,
            &unit_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    // Evidence stays but is marked STALE (INV-Q2).
    let mut evidence_ids: Vec<String> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id FROM core_evidence WHERE source_id = ?1 AND source_type != 'KNOWLEDGE'",
        )?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                evidence_ids.push(row?);
            }
        }
    }
    if !evidence_ids.is_empty() {
        counts.evidence_stale = update_freshness(
            conn,
            "core_evidence",
            "id",
            &evidence_ids,
            FreshnessState::Stale,
        )?;
        record_invalidation(
            conn,
            InvalidationArtifactType::Evidence,
            &evidence_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    // Knowledge items are first-class dependents of their source file.
    let mut knowledge_ids: Vec<String> = Vec::new();
    {
        let mut stmt =
            conn.prepare("SELECT id FROM core_knowledge_items WHERE file_occurrence_id = ?1")?;
        for occ in occurrence_ids {
            let rows = stmt.query_map(rusqlite::params![occ], |r| r.get::<_, String>(0))?;
            for row in rows {
                knowledge_ids.push(row?);
            }
        }
    }
    if !knowledge_ids.is_empty() {
        counts.knowledge_items = update_freshness(
            conn,
            "core_knowledge_items",
            "id",
            &knowledge_ids,
            FreshnessState::Invalid,
        )?;
        record_invalidation(
            conn,
            InvalidationArtifactType::KnowledgeItem,
            &knowledge_ids,
            InvalidationCause::DependencyInvalid,
            now_us,
        )?;
    }

    Ok(counts)
}

/// Per-state freshness totals over file occurrences (MCP status support).
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct FreshnessTotals {
    /// Occurrences in CURRENT state.
    pub current: i64,
    /// Occurrences in STALE state.
    pub stale: i64,
    /// Occurrences in UNKNOWN state.
    pub unknown: i64,
    /// Occurrences in INVALID state.
    pub invalid: i64,
    /// Occurrences in PENDING_REFRESH state.
    pub pending_refresh: i64,
}

/// Aggregate occurrence freshness counts across the whole database.
pub fn get_freshness_totals(conn: &Connection) -> Result<FreshnessTotals, StorageError> {
    let t = conn.query_row(
        "SELECT
             COALESCE(SUM(CASE WHEN freshness_state = 'CURRENT'          THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN freshness_state = 'STALE'            THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN freshness_state = 'UNKNOWN'          THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN freshness_state = 'INVALID'          THEN 1 ELSE 0 END), 0),
             COALESCE(SUM(CASE WHEN freshness_state = 'PENDING_REFRESH'  THEN 1 ELSE 0 END), 0)
           FROM core_file_occurrences",
        [],
        |r| {
            Ok(FreshnessTotals {
                current: r.get(0)?,
                stale: r.get(1)?,
                unknown: r.get(2)?,
                invalid: r.get(3)?,
                pending_refresh: r.get(4)?,
            })
        },
    )?;
    Ok(t)
}

// ---------------------------------------------------------------------------
// Pruning (bounded audit trail / tombstone lifecycle)
// ---------------------------------------------------------------------------

/// Microseconds in one day (matches the `*_at`/`*_us` column unit used
/// throughout this schema).
const DAY_US: i64 = 24 * 60 * 60 * 1_000_000;

/// Default retention window, in days, for `core_file_occurrences` tombstone
/// rows before [`prune_old_tombstones`] considers them eligible for deletion.
pub const DEFAULT_TOMBSTONE_RETENTION_DAYS: u32 = 90;

/// Default retention window, in days, for `core_invalidation_records` rows
/// before [`prune_old_invalidation_records`] considers them eligible for
/// deletion.
pub const DEFAULT_INVALIDATION_RECORD_RETENTION_DAYS: u32 = 90;

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

/// Permanently delete old `core_file_occurrences` tombstone rows
/// (`existence_state = 'deleted'`) so the table does not grow forever.
///
/// `core_file_occurrences` has no `deleted_at`/`updated_at` column of its
/// own (see `migrations/0001_initial.sql`) — a tombstone is born already
/// `deleted`, so its owning revision's capture time (`fo.source_revision_id`
/// → `core_source_revisions.captured_at`, always present via a `NOT NULL`
/// foreign key) IS the moment the deletion was recorded, and is used here as
/// the age basis in place of a dedicated deletion timestamp.
///
/// Five other tables carry a foreign key to `core_file_occurrences.id`
/// (`core_dependency_declarations`, `core_knowledge_items`,
/// `core_retrieval_units`, `core_structural_nodes`,
/// `core_symbol_occurrences`); with `PRAGMA foreign_keys = ON` (always set by
/// [`crate::connection::configure_connection`]) deleting a tombstone that
/// still has a live dependent in any of those tables would either raise a
/// foreign-key constraint violation (the four `NOT NULL` FKs) or leave a
/// dangling pointer (the nullable FK on `core_dependency_declarations`). This
/// function therefore only deletes tombstones with **no** live dependents in
/// any of those five tables; a tombstone with lingering dependents is simply
/// left for a later run, once whatever code path owns those dependents has
/// cleaned them up.
///
/// Returns the number of tombstone rows actually deleted.
pub fn prune_old_tombstones(
    conn: &Connection,
    older_than_days: u32,
) -> Result<usize, StorageError> {
    let cutoff_us = now_us() - i64::from(older_than_days) * DAY_US;
    let n = conn.execute(
        "DELETE FROM core_file_occurrences
          WHERE existence_state = 'deleted'
            AND EXISTS (
                SELECT 1 FROM core_source_revisions sr
                 WHERE sr.id = core_file_occurrences.source_revision_id
                   AND sr.captured_at < ?1
            )
            AND NOT EXISTS (
                SELECT 1 FROM core_dependency_declarations d
                 WHERE d.file_occurrence_id = core_file_occurrences.id
            )
            AND NOT EXISTS (
                SELECT 1 FROM core_knowledge_items k
                 WHERE k.file_occurrence_id = core_file_occurrences.id
            )
            AND NOT EXISTS (
                SELECT 1 FROM core_retrieval_units r
                 WHERE r.file_occurrence_id = core_file_occurrences.id
            )
            AND NOT EXISTS (
                SELECT 1 FROM core_structural_nodes s
                 WHERE s.file_occurrence_id = core_file_occurrences.id
            )
            AND NOT EXISTS (
                SELECT 1 FROM core_symbol_occurrences so
                 WHERE so.file_occurrence_id = core_file_occurrences.id
            )",
        rusqlite::params![cutoff_us],
    )?;
    Ok(n)
}

/// Permanently delete old `core_invalidation_records` rows so this
/// pure audit log does not grow forever.
///
/// `invalidated_at` (microseconds since Unix epoch) is the row's own
/// creation timestamp and is used directly as the age basis; rows are
/// pruned regardless of `recomputed_at` (closed or still-pending) once they
/// are older than the retention window — this table is an audit trail, not
/// a work queue, so "pending" rows do not need to be preserved indefinitely
/// either.
///
/// Returns the number of rows actually deleted.
pub fn prune_old_invalidation_records(
    conn: &Connection,
    older_than_days: u32,
) -> Result<usize, StorageError> {
    let cutoff_us = now_us() - i64::from(older_than_days) * DAY_US;
    let n = conn.execute(
        "DELETE FROM core_invalidation_records WHERE invalidated_at < ?1",
        rusqlite::params![cutoff_us],
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::fts::{NewRetrievalUnit, insert_retrieval_unit_with_fts};
    use crate::migration::run_migrations;
    use crate::repository::file_occurrence::{
        NewFileOccurrence, insert_file_occurrence, upsert_file_identity,
    };
    use crate::repository::index_generation::insert_index_generation;
    use crate::repository::repository::upsert_repository;
    use crate::repository::source_revision::insert_source_revision;
    use attic_core::{
        DiscoveryClass, ExistenceState, FileType, RepositoryId, SecurityState, SourceRevisionId,
        SourceType, SubsystemVersions,
    };
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    struct Seed {
        occ: String,
    }

    fn seed_repo_rev_occ(conn: &Connection) -> Seed {
        let repo = RepositoryId::new_v4();
        upsert_repository(conn, &repo, "/repo", "test").unwrap();
        let rev = SourceRevisionId::new_v4();
        insert_source_revision(
            conn,
            &rev,
            &repo,
            "deadbeef",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        let gen_id = attic_core::IndexGenerationId::new_v4();
        insert_index_generation(conn, &gen_id, &repo, &rev, 1, &SubsystemVersions::default())
            .unwrap();

        let fi = attic_core::FileIdentityId::new_v4();
        upsert_file_identity(conn, &fi, &repo, "basis").unwrap();
        let occ = attic_core::FileOccurrenceId::new_v4();
        insert_file_occurrence(
            conn,
            &NewFileOccurrence {
                id: &occ,
                file_identity_id: &fi,
                source_revision_id: &rev,
                index_generation_id: Some(&gen_id),
                path: "src/a.rs",
                content_hash: "h1",
                size_bytes: 10,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Clean,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();

        // One retrieval unit so propagation has something to touch.
        insert_retrieval_unit_with_fts(
            conn,
            &NewRetrievalUnit {
                id: &attic_core::RetrievalUnitId::new_v4().to_string_repr(),
                file_occurrence_id: &occ.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo.to_string_repr(),
                retrieval_text: "fn token_x() {}",
                analyzer_id: "generic",
                analyzer_version: "0.1.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        Seed {
            occ: occ.to_string_repr(),
        }
    }

    #[test]
    fn propagation_marks_units_and_writes_records() {
        let conn = migrated_conn();
        let seed = seed_repo_rev_occ(&conn);

        let counts = invalidate_for_occurrences(
            &conn,
            std::slice::from_ref(&seed.occ),
            FreshnessState::Stale,
            InvalidationCause::SourceChanged,
            1000,
        )
        .unwrap();

        assert_eq!(counts.occurrences, 1);
        assert_eq!(counts.retrieval_units, 1);
        assert!(counts.total() >= 2);

        let occ_state: String = conn
            .query_row(
                "SELECT freshness_state FROM core_file_occurrences WHERE id = ?1",
                [&seed.occ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(occ_state, "STALE");

        let unit_state: String = conn
            .query_row(
                "SELECT freshness_state FROM core_retrieval_units WHERE file_occurrence_id = ?1",
                [&seed.occ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unit_state, "INVALID");

        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_invalidation_records WHERE recomputed_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 2, "one record per touched artifact");
    }

    #[test]
    fn recompute_completes_closes_pending_records() {
        let conn = migrated_conn();
        let seed = seed_repo_rev_occ(&conn);
        invalidate_for_occurrences(
            &conn,
            std::slice::from_ref(&seed.occ),
            FreshnessState::PendingRefresh,
            InvalidationCause::SourceChanged,
            1000,
        )
        .unwrap();

        let n = record_recomputed(&conn, &seed.occ, 2000).unwrap();
        assert!(n >= 1);
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_invalidation_records
                  WHERE artifact_id = ?1 AND recomputed_at IS NULL",
                [&seed.occ],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn freshness_totals_reflect_states() {
        let conn = migrated_conn();
        let seed = seed_repo_rev_occ(&conn);
        invalidate_for_occurrences(
            &conn,
            std::slice::from_ref(&seed.occ),
            FreshnessState::Unknown,
            InvalidationCause::ReconciliationRequired,
            1000,
        )
        .unwrap();
        let t = get_freshness_totals(&conn).unwrap();
        assert_eq!(t.unknown, 1);
        assert_eq!(t.current, 0);
    }

    // -----------------------------------------------------------------------
    // Pruning
    // -----------------------------------------------------------------------

    fn set_revision_captured_at(conn: &Connection, rev_id: &str, captured_at_us: i64) {
        conn.execute(
            "UPDATE core_source_revisions SET captured_at = ?2 WHERE id = ?1",
            rusqlite::params![rev_id, captured_at_us],
        )
        .unwrap();
    }

    /// Seed a minimal repo + revision + identity + occurrence with no
    /// dependents in any FK-referencing table, so it is a clean pruning
    /// candidate on its own. Returns `(occurrence_id, source_revision_id)`.
    fn seed_bare_occurrence(
        conn: &Connection,
        existence: ExistenceState,
        path: &str,
    ) -> (String, String) {
        let repo = RepositoryId::new_v4();
        upsert_repository(conn, &repo, &format!("/repo-{path}"), "prune-test").unwrap();
        let rev = SourceRevisionId::new_v4();
        insert_source_revision(
            conn,
            &rev,
            &repo,
            "deadbeef",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        let fi = attic_core::FileIdentityId::new_v4();
        upsert_file_identity(conn, &fi, &repo, path).unwrap();
        let occ = attic_core::FileOccurrenceId::new_v4();
        insert_file_occurrence(
            conn,
            &NewFileOccurrence {
                id: &occ,
                file_identity_id: &fi,
                source_revision_id: &rev,
                index_generation_id: None,
                path,
                content_hash: "h-prune",
                size_bytes: 0,
                language: None,
                file_type: FileType::Other,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Clean,
                existence_state: existence,
            },
        )
        .unwrap();
        (occ.to_string_repr(), rev.to_string_repr())
    }

    fn occurrence_exists(conn: &Connection, occ_id: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_file_occurrences WHERE id = ?1",
                [occ_id],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    #[test]
    fn prune_old_tombstones_prunes_only_old_deleted_rows() {
        let conn = migrated_conn();
        let old_cutoff_us = now_us() - (i64::from(DEFAULT_TOMBSTONE_RETENTION_DAYS) + 1) * DAY_US;

        // Old tombstone: must be pruned.
        let (old_occ, old_rev) = seed_bare_occurrence(&conn, ExistenceState::Deleted, "old.rs");
        set_revision_captured_at(&conn, &old_rev, old_cutoff_us);

        // Recent tombstone (captured_at defaults to "now"): must survive.
        let (new_occ, _new_rev) = seed_bare_occurrence(&conn, ExistenceState::Deleted, "new.rs");

        // Live (non-deleted) row, artificially aged past the threshold: must
        // never be touched regardless of age.
        let (live_occ, live_rev) = seed_bare_occurrence(&conn, ExistenceState::Present, "live.rs");
        set_revision_captured_at(&conn, &live_rev, old_cutoff_us);

        let pruned = prune_old_tombstones(&conn, DEFAULT_TOMBSTONE_RETENTION_DAYS).unwrap();
        assert_eq!(pruned, 1, "exactly the old tombstone must be pruned");

        assert!(
            !occurrence_exists(&conn, &old_occ),
            "old tombstone must be gone"
        );
        assert!(
            occurrence_exists(&conn, &new_occ),
            "recent tombstone must survive"
        );
        assert!(
            occurrence_exists(&conn, &live_occ),
            "live row must never be pruned"
        );
    }

    #[test]
    fn prune_old_tombstones_skips_rows_with_live_dependents() {
        let conn = migrated_conn();
        let old_cutoff_us = now_us() - (i64::from(DEFAULT_TOMBSTONE_RETENTION_DAYS) + 1) * DAY_US;

        let (occ, rev) = seed_bare_occurrence(&conn, ExistenceState::Deleted, "referenced.rs");
        set_revision_captured_at(&conn, &rev, old_cutoff_us);

        // Give it a live dependent in one of the five FK-referencing tables.
        let gen_id = attic_core::IndexGenerationId::new_v4();
        let repo_row: String = conn
            .query_row(
                "SELECT fi.repository_id FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fo.id = ?1",
                [&occ],
                |r| r.get(0),
            )
            .unwrap();
        insert_index_generation(
            &conn,
            &gen_id,
            &repo_row.parse().unwrap(),
            &rev.parse().unwrap(),
            1,
            &SubsystemVersions::default(),
        )
        .unwrap();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &attic_core::RetrievalUnitId::new_v4().to_string_repr(),
                file_occurrence_id: &occ,
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_row,
                retrieval_text: "fn stale() {}",
                analyzer_id: "generic",
                analyzer_version: "0.1.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        let pruned = prune_old_tombstones(&conn, DEFAULT_TOMBSTONE_RETENTION_DAYS).unwrap();
        assert_eq!(
            pruned, 0,
            "a tombstone with a live dependent must not be pruned"
        );
        assert!(occurrence_exists(&conn, &occ));
    }

    #[test]
    fn prune_old_invalidation_records_prunes_only_old_rows() {
        let conn = migrated_conn();
        let seed = seed_repo_rev_occ(&conn);

        let old_cutoff_us =
            now_us() - (i64::from(DEFAULT_INVALIDATION_RECORD_RETENTION_DAYS) + 1) * DAY_US;
        record_invalidation(
            &conn,
            InvalidationArtifactType::FileOccurrence,
            std::slice::from_ref(&seed.occ),
            InvalidationCause::SourceChanged,
            old_cutoff_us,
        )
        .unwrap();
        record_invalidation(
            &conn,
            InvalidationArtifactType::FileOccurrence,
            std::slice::from_ref(&seed.occ),
            InvalidationCause::SourceChanged,
            now_us(),
        )
        .unwrap();

        let total_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM core_invalidation_records", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total_before, 2);

        let pruned =
            prune_old_invalidation_records(&conn, DEFAULT_INVALIDATION_RECORD_RETENTION_DAYS)
                .unwrap();
        assert_eq!(pruned, 1, "only the old record must be pruned");

        let total_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM core_invalidation_records", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(total_after, 1, "the recent record must survive");
    }
}
