//! S3 — `core_index_generations` insert, completion, and subsystem version retrieval.

use rusqlite::Connection;

use attic_core::{IndexGenerationId, RepositoryId, SourceRevisionId, SubsystemVersions};

use crate::error::StorageError;

/// Insert a new index generation record in the `started` state.
///
/// `subsystem_versions` is always explicitly provided (ADR-004).
pub fn insert_index_generation(
    conn: &Connection,
    id: &IndexGenerationId,
    repository_id: &RepositoryId,
    source_revision_id: &SourceRevisionId,
    secret_detector_version: i64,
    subsystem_versions: &SubsystemVersions,
) -> Result<(), StorageError> {
    let sv_json = subsystem_versions.to_json()?;
    conn.execute(
        "INSERT INTO core_index_generations
             (id, repository_id, source_revision_id,
              secret_detector_version, subsystem_versions_json, status)
         VALUES (?1, ?2, ?3, ?4, ?5, 'started')",
        rusqlite::params![
            id.to_string_repr(),
            repository_id.to_string_repr(),
            source_revision_id.to_string_repr(),
            secret_detector_version,
            sv_json,
        ],
    )?;
    Ok(())
}

/// Mark an index generation as completed by setting `completed_at`.
pub fn complete_index_generation(
    conn: &Connection,
    id: &IndexGenerationId,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE core_index_generations
         SET status       = 'completed',
             completed_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
         WHERE id = ?1",
        rusqlite::params![id.to_string_repr()],
    )?;
    Ok(())
}

/// Retrieve the `SubsystemVersions` for an index generation, or `None` if not found.
pub fn get_subsystem_versions(
    conn: &Connection,
    id: &IndexGenerationId,
) -> Result<Option<SubsystemVersions>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT subsystem_versions_json FROM core_index_generations WHERE id = ?1",
    )?;
    let mut rows = stmt.query(rusqlite::params![id.to_string_repr()])?;
    match rows.next()? {
        Some(row) => {
            let json: String = row.get(0)?;
            let sv = SubsystemVersions::from_json(&json)?;
            Ok(Some(sv))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::repository::upsert_repository;
    use crate::repository::source_revision::insert_source_revision;
    use attic_core::{SourceType, constants::subsystem_keys};
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn seed_repo_and_revision(conn: &Connection) -> (RepositoryId, SourceRevisionId) {
        let repo_id = RepositoryId::new_v4();
        upsert_repository(conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();
        insert_source_revision(
            conn,
            &rev_id,
            &repo_id,
            "deadbeef",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        (repo_id, rev_id)
    }

    #[test]
    fn insert_and_complete_generation() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let mut sv = SubsystemVersions::new();
        sv.set(subsystem_keys::SCHEMA, "1.0.0");
        sv.set(subsystem_keys::INDEXER, "1.0.0");

        let gen_id = IndexGenerationId::new_v4();
        insert_index_generation(&conn, &gen_id, &repo_id, &rev_id, 1, &sv).unwrap();

        // Verify status is 'started'.
        let status: String = conn
            .query_row(
                "SELECT status FROM core_index_generations WHERE id = ?1",
                rusqlite::params![gen_id.to_string_repr()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "started");

        complete_index_generation(&conn, &gen_id).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM core_index_generations WHERE id = ?1",
                rusqlite::params![gen_id.to_string_repr()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn get_subsystem_versions_round_trips() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let mut sv = SubsystemVersions::new();
        sv.set(subsystem_keys::SCHEMA, "1.0.0");
        sv.set(subsystem_keys::SECRET_DETECTOR, "1");

        let gen_id = IndexGenerationId::new_v4();
        insert_index_generation(&conn, &gen_id, &repo_id, &rev_id, 1, &sv).unwrap();

        let decoded = get_subsystem_versions(&conn, &gen_id).unwrap().unwrap();
        assert_eq!(decoded.get(subsystem_keys::SCHEMA), Some("1.0.0"));
        assert_eq!(decoded.get(subsystem_keys::SECRET_DETECTOR), Some("1"));
    }

    #[test]
    fn get_subsystem_versions_returns_none_for_unknown() {
        let conn = migrated_conn();
        let gen_id = IndexGenerationId::new_v4();
        assert!(get_subsystem_versions(&conn, &gen_id).unwrap().is_none());
    }
}
