//! Repository-removal data eviction for `attic.db`.
//!
//! `workspace remove` only drops a repository from live membership; this
//! module deletes everything it left behind. Deletion runs in small batches
//! (one writer transaction each) so a large repository never stalls the
//! shared writer queue, and in foreign-key order (`PRAGMA foreign_keys=ON`):
//! repository-wide edges first, then per-file rows, then repository-level
//! anchors, and `core_repositories` last. Every step is idempotent, so a
//! crash mid-eviction simply resumes on the next run.

use rusqlite::{Connection, params};

use crate::error::StorageError;
use crate::fts::delete_retrieval_units_for_file;

/// File occurrences removed per [`evict_repository_step`] call.
pub const EVICTION_FILE_BATCH: usize = 200;

/// Outcome of one eviction step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionStep {
    /// File occurrences deleted in this step.
    pub files: usize,
    /// Retrieval units (and their FTS rows) deleted in this step.
    pub units: usize,
    /// True once the `core_repositories` row itself is gone.
    pub complete: bool,
}

/// Run one bounded eviction step for `repository_id`. Must run inside a
/// writer-queue closure. Call repeatedly until `complete` is true.
pub fn evict_repository_step(
    conn: &Connection,
    repository_id: &str,
) -> Result<EvictionStep, StorageError> {
    let mut step = EvictionStep::default();
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM core_repositories WHERE id = ?1)",
        params![repository_id],
        |r| r.get(0),
    )?;
    if !exists {
        step.complete = true;
        return Ok(step);
    }

    // Repository-wide rows that reference file occurrences / revisions of
    // this repo, or this repo from another one. Cheap and idempotent.
    crate::crossrepo_ops::remove_repository_crossrepo_data(conn, repository_id)?;
    conn.execute(
        "DELETE FROM core_relationships
          WHERE source_repository_id = ?1 OR target_repository_id = ?1",
        params![repository_id],
    )?;
    conn.execute(
        "DELETE FROM core_dependency_declarations WHERE repository_id = ?1",
        params![repository_id],
    )?;

    // Per-file batch.
    let files: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT o.id FROM core_file_occurrences o
               JOIN core_file_identities fi ON fi.id = o.file_identity_id
              WHERE fi.repository_id = ?1
              LIMIT ?2",
        )?;
        stmt.query_map(params![repository_id, EVICTION_FILE_BATCH as i64], |r| {
            r.get(0)
        })?
        .collect::<Result<_, _>>()?
    };
    for occ in &files {
        conn.execute(
            "DELETE FROM core_retrieval_unit_nodes WHERE retrieval_unit_id IN
                (SELECT id FROM core_retrieval_units WHERE file_occurrence_id = ?1)",
            params![occ],
        )?;
        step.units += delete_retrieval_units_for_file(conn, occ)?;
        conn.execute(
            "DELETE FROM core_symbol_occurrences WHERE file_occurrence_id = ?1",
            params![occ],
        )?;
        // One statement: parent links inside the file are checked at the
        // end of the statement, so node order does not matter.
        conn.execute(
            "DELETE FROM core_structural_nodes WHERE file_occurrence_id = ?1",
            params![occ],
        )?;
        conn.execute(
            "DELETE FROM core_file_occurrences WHERE id = ?1",
            params![occ],
        )?;
    }
    step.files = files.len();
    if !files.is_empty() {
        return Ok(step);
    }

    // No file occurrences left: sweep any units/nodes not reachable through
    // a file (defensive), then repository-level anchors, repository last.
    let orphan_units: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT rowid, retrieval_text FROM core_retrieval_units WHERE repository_id = ?1",
        )?;
        stmt.query_map(params![repository_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?
    };
    for (rowid, text) in &orphan_units {
        crate::fts::fts_retrieval_unit_delete(conn, *rowid, text)?;
    }
    let rid = repository_id;
    for sql in [
        "DELETE FROM core_retrieval_unit_nodes WHERE retrieval_unit_id IN
            (SELECT id FROM core_retrieval_units WHERE repository_id = ?1)",
        "DELETE FROM core_retrieval_units WHERE repository_id = ?1",
        "DELETE FROM core_retrieval_unit_nodes WHERE structural_node_id IN
            (SELECT id FROM core_structural_nodes WHERE repository_id = ?1)",
        "DELETE FROM core_structural_nodes WHERE repository_id = ?1",
        "DELETE FROM core_symbol_occurrences WHERE symbol_identity_id IN
            (SELECT id FROM core_symbol_identities WHERE repository_id = ?1)",
        "DELETE FROM core_symbol_identities WHERE repository_id = ?1",
        "DELETE FROM core_identity_links WHERE repository_id = ?1",
        "DELETE FROM core_file_identities WHERE repository_id = ?1",
        "DELETE FROM index_analysis_cache WHERE repository_id = ?1",
        "DELETE FROM core_workspace_snapshot_revisions WHERE repository_id = ?1",
        "DELETE FROM core_index_generations WHERE source_revision_id IN
            (SELECT id FROM core_source_revisions WHERE repository_id = ?1)",
        "DELETE FROM core_source_revisions WHERE repository_id = ?1",
        "DELETE FROM ops_tasks WHERE repository_id = ?1",
        "DELETE FROM core_repositories WHERE id = ?1",
    ] {
        conn.execute(sql, params![rid])?;
    }
    step.units += orphan_units.len();
    step.complete = true;
    Ok(step)
}
