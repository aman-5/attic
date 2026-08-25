//! `core_identity_links` — explicit cross-revision file-identity continuity
//! records (ADR-009; identity contract §Confidence Levels).
//!
//! Identity rows are never mutated by Phase 2; links are a separate,
//! always-confidence-labelled record so an uncertain rename can never be
//! silently promoted to exact identity.

use rusqlite::Connection;

use attic_core::{FileIdentityId, RepositoryId};

use crate::error::StorageError;

/// Confidence values (identity contract): EXACT | HEURISTIC | NONE.
pub mod confidence {
    /// Deterministic match — reserved for Git rename detection (not wired in
    /// Phase 2; no Git plumbing dependency).
    pub const EXACT: &str = "EXACT";
    /// Plausible content-based match; never claimed as certain.
    pub const HEURISTIC: &str = "HEURISTIC";
    /// No continuity.
    pub const NONE: &str = "NONE";
}

/// Basis values for a link.
pub mod basis {
    /// Git-reported rename (`R<score>`) — not wired in Phase 2.
    pub const GIT_RENAME: &str = "GIT_RENAME";
    /// Identical BLAKE3 content hash across the paired paths.
    pub const CONTENT_MATCH: &str = "CONTENT_MATCH";
    /// No deterministic basis.
    pub const NONE: &str = "NONE";
}

/// One identity-continuation record to insert.
pub struct NewIdentityLink<'a> {
    /// Primary key UUID string.
    pub id: &'a str,
    /// Repository both identities belong to.
    pub repository_id: &'a RepositoryId,
    /// Identity observed at `prior_path`.
    pub from_identity_id: &'a FileIdentityId,
    /// Identity observed at `new_path`.
    pub to_identity_id: &'a FileIdentityId,
    /// Repo-relative path before the move/rename.
    pub prior_path: &'a str,
    /// Repo-relative path after the move/rename.
    pub new_path: &'a str,
    /// [`confidence`] value.
    pub confidence: &'a str,
    /// [`basis`] value.
    pub basis: &'a str,
    /// Creation time (microseconds since Unix epoch).
    pub created_at: i64,
}

/// Insert one identity link row.
pub fn insert_identity_link(
    conn: &Connection,
    link: &NewIdentityLink<'_>,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO core_identity_links
             (id, repository_id, from_identity_id, to_identity_id,
              prior_path, new_path, confidence, basis, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            link.id,
            link.repository_id.to_string_repr(),
            link.from_identity_id.to_string_repr(),
            link.to_identity_id.to_string_repr(),
            link.prior_path,
            link.new_path,
            link.confidence,
            link.basis,
            link.created_at
        ],
    )?;
    Ok(())
}

/// Most recent inbound link for an identity, if any.
///
/// Returns `(from_identity_id, prior_path, new_path, confidence)`.
pub fn latest_link_for_identity(
    conn: &Connection,
    to_identity_id: &FileIdentityId,
) -> Result<Option<(String, String, String, String)>, StorageError> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT from_identity_id, prior_path, new_path, confidence
           FROM core_identity_links
          WHERE to_identity_id = ?1
          ORDER BY created_at DESC, id DESC LIMIT 1",
        rusqlite::params![to_identity_id.to_string_repr()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
    .map_err(StorageError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::file_occurrence::upsert_file_identity;
    use rusqlite::Connection;

    #[test]
    fn insert_and_read_back_link() {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let repo = RepositoryId::new_v4();
        crate::repository::repository::upsert_repository(&conn, &repo, "/repo", "r").unwrap();
        let from = FileIdentityId::new_v4();
        let to = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &from, &repo, "b-from").unwrap();
        upsert_file_identity(&conn, &to, &repo, "b-to").unwrap();

        insert_identity_link(
            &conn,
            &NewIdentityLink {
                id: &uuid::Uuid::new_v4().to_string(),
                repository_id: &repo,
                from_identity_id: &from,
                to_identity_id: &to,
                prior_path: "src/old.rs",
                new_path: "src/new.rs",
                confidence: confidence::HEURISTIC,
                basis: basis::CONTENT_MATCH,
                created_at: 1234,
            },
        )
        .unwrap();

        let link = latest_link_for_identity(&conn, &to)
            .unwrap()
            .expect("link exists");
        assert_eq!(link.0, from.to_string_repr());
        assert_eq!(link.1, "src/old.rs");
        assert_eq!(link.2, "src/new.rs");
        assert_eq!(link.3, confidence::HEURISTIC);
    }
}
