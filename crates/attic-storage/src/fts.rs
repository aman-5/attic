//! S5 — FTS5 external-content table helpers: insert, delete, update, and search.
//!
//! The `fts_retrieval_units` FTS5 table is declared as an **external-content**
//! table mirroring `core_retrieval_units.retrieval_text`.  Callers keep them
//! in sync via the helpers below.
//!
//! **INVARIANT**: Secret bytes MUST NEVER appear in `retrieval_text` or in
//! any FTS index entry.  All content indexed here has already passed through
//! the Phase 1B secret-scan / redaction gate.
//!
//! # Search contract
//! - Results are bounded by `max_results` (hard-capped at [`MAX_SEARCH_RESULTS`]).
//! - Ordering is deterministic: BM25 score ASC (lower/more-negative = better),
//!   then `rowid` ASC as the tie-breaker.
//! - All filters use parameterized SQL; query strings are never interpolated.
//! - Ghost results after deletion are impossible because:
//!   (a) the FTS5 `'delete'` protocol is used for removal, and
//!   (b) the INNER JOIN with `core_retrieval_units` acts as a double-check.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------------

/// Hard cap on the number of FTS search results returned.
///
/// Callers may request fewer; requests above this cap are silently clamped.
pub const MAX_SEARCH_RESULTS: usize = 200;

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// A single FTS search result row with full provenance.
///
/// This is a Phase 1D lexical search result.  It is **not** validated
/// Phase 4 Evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FtsSearchResult {
    /// `core_retrieval_units.id` (UUID string).
    pub retrieval_unit_id: String,
    /// `core_file_occurrences.id` (UUID string).
    pub file_occurrence_id: String,
    /// `core_index_generations.id` (UUID string) — IndexGeneration provenance.
    pub index_generation_id: String,
    /// `core_repositories.id` (UUID string) — SourceRevision provenance.
    pub repository_id: String,
    /// Repository display name.
    pub repository_name: String,
    /// Workspace-relative file path.
    pub path: String,
    /// Language identifier, if known.
    pub language: Option<String>,
    /// File type string (e.g. `"rust"`, `"python"`).
    pub file_type: String,
    /// The body text already indexed (safe — never contains secrets).
    pub body: String,
    /// Positive relevance score: higher = more relevant.
    /// (SQLite FTS5 BM25 is negative; we negate it here.)
    pub score: f64,
    /// Start line (0-based) of the retrieval unit span, if recorded.
    pub start_line: Option<u32>,
    /// End line (0-based, inclusive) of the retrieval unit span, if recorded.
    pub end_line: Option<u32>,
}

// ---------------------------------------------------------------------------
// fts_retrieval_units — low-level insert / delete / update
// ---------------------------------------------------------------------------

/// Insert a row into the `fts_retrieval_units` external-content FTS5 table.
///
/// `rowid` must match the integer primary key of the corresponding
/// `core_retrieval_units` row.
pub fn fts_retrieval_unit_insert(
    conn: &Connection,
    rowid: i64,
    retrieval_text: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_retrieval_units(rowid, retrieval_text) VALUES (?1, ?2)",
        params![rowid, retrieval_text],
    )?;
    Ok(())
}

/// Remove a row from the `fts_retrieval_units` FTS5 table using the
/// external-content `'delete'` protocol.
///
/// `old_retrieval_text` must be the **current** indexed text of the row
/// (required by FTS5 external-content delete semantics).
pub fn fts_retrieval_unit_delete(
    conn: &Connection,
    rowid: i64,
    old_retrieval_text: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_retrieval_units(fts_retrieval_units, rowid, retrieval_text)
         VALUES ('delete', ?1, ?2)",
        params![rowid, old_retrieval_text],
    )?;
    Ok(())
}

/// Update a row in the `fts_retrieval_units` FTS5 table.
///
/// Deletes the old content then inserts the new content.
pub fn fts_retrieval_unit_update(
    conn: &Connection,
    rowid: i64,
    old_retrieval_text: &str,
    new_retrieval_text: &str,
) -> Result<(), StorageError> {
    fts_retrieval_unit_delete(conn, rowid, old_retrieval_text)?;
    fts_retrieval_unit_insert(conn, rowid, new_retrieval_text)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// fts_symbol_names — low-level insert / delete
// ---------------------------------------------------------------------------

/// Insert a row into the `fts_symbol_names` external-content FTS5 table.
pub fn fts_symbol_name_insert(
    conn: &Connection,
    rowid: i64,
    qualified_name: &str,
    kind: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_symbol_names(rowid, qualified_name, kind) VALUES (?1, ?2, ?3)",
        params![rowid, qualified_name, kind],
    )?;
    Ok(())
}

/// Remove a row from the `fts_symbol_names` FTS5 table.
pub fn fts_symbol_name_delete(
    conn: &Connection,
    rowid: i64,
    old_qualified_name: &str,
    old_kind: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO fts_symbol_names(fts_symbol_names, rowid, qualified_name, kind)
         VALUES ('delete', ?1, ?2, ?3)",
        params![rowid, old_qualified_name, old_kind],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Retrieval unit — insert with FTS sync
// ---------------------------------------------------------------------------

/// A retrieval unit record ready for insertion.
#[derive(Debug, Clone)]
pub struct NewRetrievalUnit<'a> {
    /// Stable UUID for this retrieval unit.
    pub id: &'a str,
    /// FK → `core_file_occurrences.id`.
    pub file_occurrence_id: &'a str,
    /// FK → `core_index_generations.id`.
    pub index_generation_id: &'a str,
    /// FK → `core_repositories.id` (denormalized for search efficiency).
    pub repository_id: &'a str,
    /// Safe retrieval text (must not contain secrets).
    pub retrieval_text: &'a str,
    /// Analyzer identifier that produced this unit.
    pub analyzer_id: &'a str,
    /// Analyzer version that produced this unit.
    pub analyzer_version: &'a str,
    /// Start line within the file (0-based).
    pub start_line: Option<u32>,
    /// End line within the file (0-based, inclusive).
    pub end_line: Option<u32>,
    /// Whether this unit's text was redacted by Phase 1B.
    pub is_redacted: bool,
}

/// Insert a retrieval unit into `core_retrieval_units` and synchronize
/// the FTS index atomically within the caller's transaction.
///
/// Returns the SQLite rowid of the newly inserted row.
pub fn insert_retrieval_unit_with_fts(
    conn: &Connection,
    unit: &NewRetrievalUnit<'_>,
) -> Result<i64, StorageError> {
    conn.execute(
        "INSERT INTO core_retrieval_units
             (id, repository_id, file_occurrence_id, index_generation_id,
              retrieval_text, analyzer_id, analyzer_version,
              start_line, end_line, is_redacted,
              lexical_state, semantic_state, freshness_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                 'CURRENT', 'NONE', 'CURRENT')",
        rusqlite::params![
            unit.id,
            unit.repository_id,
            unit.file_occurrence_id,
            unit.index_generation_id,
            unit.retrieval_text,
            unit.analyzer_id,
            unit.analyzer_version,
            unit.start_line,
            unit.end_line,
            unit.is_redacted as i32,
        ],
    )?;
    let rowid = conn.last_insert_rowid();
    fts_retrieval_unit_insert(conn, rowid, unit.retrieval_text)?;
    Ok(rowid)
}

/// Delete a retrieval unit by UUID and remove it from the FTS index.
///
/// Returns `Ok(false)` if the row was not found.
pub fn delete_retrieval_unit_with_fts(
    conn: &Connection,
    id: &str,
) -> Result<bool, StorageError> {
    let maybe = conn.query_row(
        "SELECT rowid, retrieval_text FROM core_retrieval_units WHERE id = ?1",
        rusqlite::params![id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    );

    match maybe {
        Ok((rowid, text)) => {
            fts_retrieval_unit_delete(conn, rowid, &text)?;
            conn.execute(
                "DELETE FROM core_retrieval_units WHERE id = ?1",
                rusqlite::params![id],
            )?;
            Ok(true)
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
        Err(e) => Err(StorageError::Sqlite(e)),
    }
}

/// Delete all retrieval units for a given file occurrence and remove them
/// from the FTS index.  Used when re-indexing a file.
pub fn delete_retrieval_units_for_file(
    conn: &Connection,
    file_occurrence_id: &str,
) -> Result<usize, StorageError> {
    let rows: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT rowid, retrieval_text FROM core_retrieval_units
             WHERE file_occurrence_id = ?1",
        )?;
        let iter = stmt.query_map(rusqlite::params![file_occurrence_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        iter.collect::<Result<Vec<_>, _>>()?
    };

    let count = rows.len();
    for (rowid, text) in &rows {
        fts_retrieval_unit_delete(conn, *rowid, text)?;
    }
    conn.execute(
        "DELETE FROM core_retrieval_units WHERE file_occurrence_id = ?1",
        rusqlite::params![file_occurrence_id],
    )?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// FTS search
// ---------------------------------------------------------------------------

/// Parameters for a full-text search query.
#[derive(Debug, Clone)]
pub struct FtsSearchParams<'a> {
    /// FTS5 query string (treated as a MATCH expression).
    pub query: &'a str,
    /// Optional repository UUID filter.
    pub repository_id: Option<&'a str>,
    /// Optional file type filter (e.g. `"rust"`, `"python"`).
    pub file_type: Option<&'a str>,
    /// Optional language filter.
    pub language: Option<&'a str>,
    /// Maximum number of results.  Clamped to [`MAX_SEARCH_RESULTS`].
    pub max_results: usize,
}

/// Execute a bounded, deterministic FTS5 lexical search over retrieval units.
///
/// # Security
/// - `query` is passed as a parameterized FTS5 MATCH operand.
/// - All filters use parameterized bindings — never string interpolation.
pub fn fts_search(
    conn: &Connection,
    p: &FtsSearchParams<'_>,
) -> Result<Vec<FtsSearchResult>, StorageError> {
    let limit = p.max_results.min(MAX_SEARCH_RESULTS) as i64;

    // We JOIN fts back to the base table so deleted-but-not-yet-purged FTS
    // entries are excluded via the INNER JOIN condition.
    // Ordering: bm25() is negative (lower = better), so ORDER BY ASC gives
    // "best first"; rowid ASC breaks ties deterministically.
    let sql = "
        SELECT
            r.id                     AS retrieval_unit_id,
            r.file_occurrence_id     AS file_occurrence_id,
            r.index_generation_id    AS index_generation_id,
            COALESCE(r.repository_id, sr.repository_id) AS repository_id,
            COALESCE(repo2.display_name, repo1.display_name, '') AS repository_name,
            fo.path                  AS path,
            fo.language              AS language,
            fo.file_type             AS file_type,
            r.retrieval_text         AS body,
            bm25(fts_retrieval_units) AS score,
            r.start_line             AS start_line,
            r.end_line               AS end_line
        FROM fts_retrieval_units fts
        INNER JOIN core_retrieval_units  r    ON r.rowid = fts.rowid
        INNER JOIN core_file_occurrences fo   ON fo.id   = r.file_occurrence_id
        INNER JOIN core_source_revisions sr   ON sr.id   = fo.source_revision_id
        LEFT  JOIN core_repositories     repo1 ON repo1.id = sr.repository_id
        LEFT  JOIN core_repositories     repo2 ON repo2.id = r.repository_id
        WHERE fts_retrieval_units MATCH ?1
          AND (?2 IS NULL OR COALESCE(r.repository_id, sr.repository_id) = ?2)
          AND (?3 IS NULL OR fo.file_type = ?3)
          AND (?4 IS NULL OR fo.language  = ?4)
        ORDER BY bm25(fts_retrieval_units) ASC, fts.rowid ASC
        LIMIT ?5
    ";

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        rusqlite::params![p.query, p.repository_id, p.file_type, p.language, limit],
        |row| {
            let raw_score: f64 = row.get("score")?;
            Ok(FtsSearchResult {
                retrieval_unit_id: row.get("retrieval_unit_id")?,
                file_occurrence_id: row.get("file_occurrence_id")?,
                index_generation_id: row.get("index_generation_id")?,
                repository_id: row.get("repository_id")?,
                repository_name: row.get("repository_name")?,
                path: row.get("path")?,
                language: row.get("language")?,
                file_type: row.get("file_type")?,
                body: row.get("body")?,
                score: -raw_score,
                start_line: row.get("start_line")?,
                end_line: row.get("end_line")?,
            })
        },
    )?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Execute an exact path lookup: return all retrieval units for a specific
/// workspace-relative file path, optionally scoped to a repository.
///
/// Results are ordered by `start_line ASC, rowid ASC`.
pub fn fts_path_lookup(
    conn: &Connection,
    path: &str,
    repository_id: Option<&str>,
    max_results: usize,
) -> Result<Vec<FtsSearchResult>, StorageError> {
    let limit = max_results.min(MAX_SEARCH_RESULTS) as i64;

    let sql = "
        SELECT
            r.id                     AS retrieval_unit_id,
            r.file_occurrence_id     AS file_occurrence_id,
            r.index_generation_id    AS index_generation_id,
            COALESCE(r.repository_id, sr.repository_id) AS repository_id,
            COALESCE(repo2.display_name, repo1.display_name, '') AS repository_name,
            fo.path                  AS path,
            fo.language              AS language,
            fo.file_type             AS file_type,
            r.retrieval_text         AS body,
            0.0                      AS score,
            r.start_line             AS start_line,
            r.end_line               AS end_line
        FROM core_retrieval_units  r
        INNER JOIN core_file_occurrences fo   ON fo.id   = r.file_occurrence_id
        INNER JOIN core_source_revisions sr   ON sr.id   = fo.source_revision_id
        LEFT  JOIN core_repositories     repo1 ON repo1.id = sr.repository_id
        LEFT  JOIN core_repositories     repo2 ON repo2.id = r.repository_id
        WHERE fo.path = ?1
          AND (?2 IS NULL OR COALESCE(r.repository_id, sr.repository_id) = ?2)
        ORDER BY r.start_line ASC, r.rowid ASC
        LIMIT ?3
    ";

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        rusqlite::params![path, repository_id, limit],
        |row| {
            Ok(FtsSearchResult {
                retrieval_unit_id: row.get("retrieval_unit_id")?,
                file_occurrence_id: row.get("file_occurrence_id")?,
                index_generation_id: row.get("index_generation_id")?,
                repository_id: row.get("repository_id")?,
                repository_name: row.get("repository_name")?,
                path: row.get("path")?,
                language: row.get("language")?,
                file_type: row.get("file_type")?,
                body: row.get("body")?,
                score: 0.0,
                start_line: row.get("start_line")?,
                end_line: row.get("end_line")?,
            })
        },
    )?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::repository::upsert_repository;
    use crate::repository::source_revision::insert_source_revision;
    use crate::repository::index_generation::insert_index_generation;
    use crate::repository::file_occurrence::{
        upsert_file_identity, insert_file_occurrence, NewFileOccurrence,
    };
    use attic_core::{
        DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
        IndexGenerationId, RepositoryId, SecurityState, SourceRevisionId, SourceType,
        SubsystemVersions,
    };
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    /// Seed a minimal repository + revision + generation and return their IDs.
    fn seed_repo_rev_gen(
        conn: &Connection,
        root: &str,
    ) -> (RepositoryId, SourceRevisionId, IndexGenerationId) {
        let repo_id = RepositoryId::new_v4();
        upsert_repository(conn, &repo_id, root, "test-repo").unwrap();

        let rev_id = SourceRevisionId::new_v4();
        insert_source_revision(
            conn,
            &rev_id,
            &repo_id,
            "abc123",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();

        let gen_id = IndexGenerationId::new_v4();
        let sv = SubsystemVersions::new();
        insert_index_generation(conn, &gen_id, &repo_id, &rev_id, 1, &sv).unwrap();

        (repo_id, rev_id, gen_id)
    }

    fn seed_file(
        conn: &Connection,
        repo_id: &RepositoryId,
        rev_id: &SourceRevisionId,
        gen_id: &IndexGenerationId,
        path: &str,
    ) -> FileOccurrenceId {
        let fid = FileIdentityId::new_v4();
        upsert_file_identity(conn, &fid, repo_id, "basis").unwrap();
        let occ_id = FileOccurrenceId::new_v4();
        insert_file_occurrence(
            conn,
            &NewFileOccurrence {
                id: &occ_id,
                file_identity_id: &fid,
                source_revision_id: rev_id,
                index_generation_id: Some(gen_id),
                path,
                content_hash: "blake3:aa",
                size_bytes: 128,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Clean,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();
        occ_id
    }

    // -----------------------------------------------------------------------
    // FTS insert + search
    // -----------------------------------------------------------------------

    #[test]
    fn fts_insert_and_search_finds_text() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/a");
        let occ_id = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/lib.rs");

        let unit_id = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &unit_id,
                file_occurrence_id: &occ_id.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "fn hello_world() {}",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        let results = fts_search(
            &conn,
            &FtsSearchParams {
                query: "hello_world",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].retrieval_unit_id, unit_id);
    }

    // -----------------------------------------------------------------------
    // FTS delete — no ghost results
    // -----------------------------------------------------------------------

    #[test]
    fn fts_delete_removes_from_search() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/b");
        let occ_id = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/gone.rs");

        let unit_id = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &unit_id,
                file_occurrence_id: &occ_id.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "unique_ghost_phrase_xyz",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: None,
                end_line: None,
                is_redacted: false,
            },
        )
        .unwrap();

        // Verify present before delete.
        let before = fts_search(
            &conn,
            &FtsSearchParams {
                query: "unique_ghost_phrase_xyz",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(before.len(), 1, "should find before delete");

        let deleted = delete_retrieval_unit_with_fts(&conn, &unit_id).unwrap();
        assert!(deleted);

        // Must be gone after delete — no ghost.
        let after = fts_search(
            &conn,
            &FtsSearchParams {
                query: "unique_ghost_phrase_xyz",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(after.len(), 0, "ghost result after delete");
    }

    // -----------------------------------------------------------------------
    // FTS update — old text gone, new text findable
    // -----------------------------------------------------------------------

    #[test]
    fn fts_update_replaces_indexed_text() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/c");
        let occ_id = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/update.rs");

        let unit_id = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &unit_id,
                file_occurrence_id: &occ_id.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "old_function_alpha",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: None,
                end_line: None,
                is_redacted: false,
            },
        )
        .unwrap();

        // Fetch rowid for update.
        let rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM core_retrieval_units WHERE id = ?1",
                rusqlite::params![unit_id],
                |r| r.get(0),
            )
            .unwrap();

        fts_retrieval_unit_update(&conn, rowid, "old_function_alpha", "new_function_beta")
            .unwrap();

        // Old text must be gone.
        let old_hits = fts_search(
            &conn,
            &FtsSearchParams {
                query: "old_function_alpha",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(old_hits.len(), 0, "old text must be gone after update");

        // New text must be findable.
        let new_hits = fts_search(
            &conn,
            &FtsSearchParams {
                query: "new_function_beta",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(new_hits.len(), 1, "new text must be findable after update");
    }

    // -----------------------------------------------------------------------
    // Repository-scoped search
    // -----------------------------------------------------------------------

    #[test]
    fn fts_search_repository_scope_filter() {
        let conn = migrated_conn();
        let (repo_a, rev_a, gen_a) = seed_repo_rev_gen(&conn, "/repo/alpha");
        let (repo_b, rev_b, gen_b) = seed_repo_rev_gen(&conn, "/repo/beta");
        let occ_a = seed_file(&conn, &repo_a, &rev_a, &gen_a, "alpha/lib.rs");
        let occ_b = seed_file(&conn, &repo_b, &rev_b, &gen_b, "beta/lib.rs");

        let uid_a = uuid::Uuid::new_v4().to_string();
        let uid_b = uuid::Uuid::new_v4().to_string();

        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &uid_a,
                file_occurrence_id: &occ_a.to_string_repr(),
                index_generation_id: &gen_a.to_string_repr(),
                repository_id: &repo_a.to_string_repr(),
                retrieval_text: "fn scope_filter_token() {}",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &uid_b,
                file_occurrence_id: &occ_b.to_string_repr(),
                index_generation_id: &gen_b.to_string_repr(),
                repository_id: &repo_b.to_string_repr(),
                retrieval_text: "fn scope_filter_token() {}",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        // Unscoped — both should appear.
        let all = fts_search(
            &conn,
            &FtsSearchParams {
                query: "scope_filter_token",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(all.len(), 2, "unscoped search must find both repos");

        // Scoped to repo_a — only uid_a.
        let scoped = fts_search(
            &conn,
            &FtsSearchParams {
                query: "scope_filter_token",
                repository_id: Some(&repo_a.to_string_repr()),
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(scoped.len(), 1, "repo-scoped search must return 1 result");
        assert_eq!(scoped[0].retrieval_unit_id, uid_a);
    }

    // -----------------------------------------------------------------------
    // Bounded result count
    // -----------------------------------------------------------------------

    #[test]
    fn fts_bounded_result_count() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/bounded");
        let occ_id = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/lib.rs");

        for i in 0..10u32 {
            let uid = uuid::Uuid::new_v4().to_string();
            insert_retrieval_unit_with_fts(
                &conn,
                &NewRetrievalUnit {
                    id: &uid,
                    file_occurrence_id: &occ_id.to_string_repr(),
                    index_generation_id: &gen_id.to_string_repr(),
                    repository_id: &repo_id.to_string_repr(),
                    retrieval_text: &format!("fn bounded_item_{i}() {{ /* common_shared_token */ }}"),
                    analyzer_id: "generic",
                    analyzer_version: "1.0.0",
                    start_line: Some(i),
                    end_line: Some(i),
                    is_redacted: false,
                },
            )
            .unwrap();
        }

        let results = fts_search(
            &conn,
            &FtsSearchParams {
                query: "common_shared_token",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 3,
            },
        )
        .unwrap();
        assert_eq!(results.len(), 3, "result count must respect max_results");
    }

    // -----------------------------------------------------------------------
    // Path lookup — exact path
    // -----------------------------------------------------------------------

    #[test]
    fn fts_path_lookup_exact() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/path");
        let occ_target = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/target.rs");
        let occ_other = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/other.rs");

        for i in 0..3u32 {
            let uid = uuid::Uuid::new_v4().to_string();
            insert_retrieval_unit_with_fts(
                &conn,
                &NewRetrievalUnit {
                    id: &uid,
                    file_occurrence_id: &occ_target.to_string_repr(),
                    index_generation_id: &gen_id.to_string_repr(),
                    repository_id: &repo_id.to_string_repr(),
                    retrieval_text: &format!("fn target_fn_{i}() {{}}"),
                    analyzer_id: "generic",
                    analyzer_version: "1.0.0",
                    start_line: Some(i * 10),
                    end_line: Some(i * 10 + 2),
                    is_redacted: false,
                },
            )
            .unwrap();
        }

        let uid_other = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &uid_other,
                file_occurrence_id: &occ_other.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "fn other_fn() {}",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(1),
                is_redacted: false,
            },
        )
        .unwrap();

        let results = fts_path_lookup(&conn, "src/target.rs", None, 10).unwrap();
        assert_eq!(results.len(), 3, "path lookup should return all 3 units for target.rs");
        for r in &results {
            assert_eq!(r.path, "src/target.rs");
        }
        let lines: Vec<Option<u32>> = results.iter().map(|r| r.start_line).collect();
        assert!(lines.windows(2).all(|w| w[0] <= w[1]), "results not ordered by start_line");
    }

    // -----------------------------------------------------------------------
    // delete_retrieval_units_for_file removes all units for that file
    // -----------------------------------------------------------------------

    #[test]
    fn fts_delete_retrieval_units_for_file_removes_all() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/del");
        let occ_target = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/del.rs");
        let occ_keep = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/keep.rs");

        let target_oid = occ_target.to_string_repr();
        let keep_oid = occ_keep.to_string_repr();

        for i in 0..3u32 {
            let uid = uuid::Uuid::new_v4().to_string();
            insert_retrieval_unit_with_fts(
                &conn,
                &NewRetrievalUnit {
                    id: &uid,
                    file_occurrence_id: &target_oid,
                    index_generation_id: &gen_id.to_string_repr(),
                    repository_id: &repo_id.to_string_repr(),
                    retrieval_text: &format!("fn del_fn_{i}() {{ /* del_unique_tok */ }}"),
                    analyzer_id: "generic",
                    analyzer_version: "1.0.0",
                    start_line: Some(i),
                    end_line: Some(i),
                    is_redacted: false,
                },
            )
            .unwrap();
        }

        let uid_keep = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &uid_keep,
                file_occurrence_id: &keep_oid,
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "fn keep_fn() { /* del_unique_tok */ }",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(0),
                is_redacted: false,
            },
        )
        .unwrap();

        let deleted = delete_retrieval_units_for_file(&conn, &target_oid).unwrap();
        assert_eq!(deleted, 3, "should have deleted 3 units");

        let del_results = fts_search(
            &conn,
            &FtsSearchParams {
                query: "del_fn_0",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert!(del_results.is_empty(), "del.rs FTS entries should be purged");

        let keep_results = fts_search(
            &conn,
            &FtsSearchParams {
                query: "keep_fn",
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 10,
            },
        )
        .unwrap();
        assert_eq!(keep_results.len(), 1, "keep.rs unit must survive deletion of del.rs");
        assert_eq!(keep_results[0].retrieval_unit_id, uid_keep);
    }

    // -----------------------------------------------------------------------
    // Redacted content: is_redacted flag round-trips; placeholder stored
    // -----------------------------------------------------------------------

    #[test]
    fn fts_redacted_unit_stores_placeholder_not_secret() {
        let conn = migrated_conn();
        let (repo_id, rev_id, gen_id) = seed_repo_rev_gen(&conn, "/repo/redact");
        let occ_id = seed_file(&conn, &repo_id, &rev_id, &gen_id, "src/secret.rs");

        let uid = uuid::Uuid::new_v4().to_string();
        insert_retrieval_unit_with_fts(
            &conn,
            &NewRetrievalUnit {
                id: &uid,
                file_occurrence_id: &occ_id.to_string_repr(),
                index_generation_id: &gen_id.to_string_repr(),
                repository_id: &repo_id.to_string_repr(),
                retrieval_text: "[REDACTED]",
                analyzer_id: "generic",
                analyzer_version: "1.0.0",
                start_line: Some(0),
                end_line: Some(5),
                is_redacted: true,
            },
        )
        .unwrap();

        let path_results = fts_path_lookup(&conn, "src/secret.rs", None, 10).unwrap();
        assert_eq!(path_results.len(), 1);
        assert_eq!(
            path_results[0].body, "[REDACTED]",
            "redacted unit body must be [REDACTED] placeholder"
        );

        let is_redacted: i32 = conn
            .query_row(
                "SELECT is_redacted FROM core_retrieval_units WHERE id = ?1",
                rusqlite::params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(is_redacted, 1, "is_redacted column must be 1");
    }
}
