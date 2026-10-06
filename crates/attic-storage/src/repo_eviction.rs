//! Repository-removal data eviction for `attic.db`.
//!
//! `workspace remove` only drops a repository from live membership; this
//! module deletes everything it left behind. Deletion runs in small batches
//! (one writer transaction each) so a large repository never stalls the
//! shared writer queue, and in foreign-key order (`PRAGMA foreign_keys=ON`):
//! repository-wide edges first, then per-file rows, then repository-level
//! anchors, and `core_repositories` last. Every step is idempotent, so a
//! crash mid-eviction simply resumes on the next run.

use std::path::{Path, PathBuf};

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

fn normalize_root_key(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let s = s
        .trim_start_matches("//?/")
        .trim_end_matches('/')
        .to_string();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

fn ensure_workspace_membership_table(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS workspace_active_roots
            (root_key TEXT PRIMARY KEY);",
    )?;
    Ok(())
}

/// Replace the writer-connection snapshot of active workspace roots.
pub fn sync_workspace_membership(
    conn: &Connection,
    active_roots: &[PathBuf],
) -> Result<(), StorageError> {
    ensure_workspace_membership_table(conn)?;
    conn.execute("DELETE FROM temp.workspace_active_roots", [])?;
    let mut insert =
        conn.prepare("INSERT OR REPLACE INTO temp.workspace_active_roots (root_key) VALUES (?1)")?;
    for root in active_roots {
        insert.execute(params![normalize_root_key(root)])?;
    }
    Ok(())
}

/// True when `root_path` is covered by the current writer-side membership
/// snapshot (either exactly configured or nested under a configured
/// container root).
pub fn workspace_membership_contains(
    conn: &Connection,
    root_path: &Path,
) -> Result<bool, StorageError> {
    ensure_workspace_membership_table(conn)?;
    let root_key = normalize_root_key(root_path);
    let mut stmt = conn.prepare("SELECT root_key FROM temp.workspace_active_roots")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let configured_key: String = row.get(0)?;
        if root_key == configured_key || root_key.starts_with(&format!("{configured_key}/")) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Run one bounded eviction step only if `root_path` is STILL inactive in
/// the writer-side membership snapshot. Returns `None` when a re-add raced
/// in before this transaction began, so the caller can cancel the task
/// without deleting anything further.
pub fn evict_repository_step_if_inactive(
    conn: &Connection,
    repository_id: &str,
    root_path: &Path,
) -> Result<Option<EvictionStep>, StorageError> {
    if workspace_membership_contains(conn, root_path)? {
        return Ok(None);
    }
    evict_repository_step(conn, repository_id).map(Some)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migration::run_migrations;
    use crate::repository::repository::{lookup_repository_by_root_path, upsert_repository};
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn guarded_step_skips_repository_under_active_container_root() {
        let conn = migrated_conn();
        let configured_root = PathBuf::from(r"C:\workspace");
        let repo_root = configured_root.join("repo");
        let repo_id = attic_core::RepositoryId::new_v4();
        upsert_repository(&conn, &repo_id, &repo_root.to_string_lossy(), "repo-active").unwrap();
        sync_workspace_membership(&conn, &[configured_root]).unwrap();

        let step =
            evict_repository_step_if_inactive(&conn, &repo_id.to_string(), &repo_root).unwrap();
        assert!(
            step.is_none(),
            "an active container root must cancel the step before any delete"
        );
        assert_eq!(
            lookup_repository_by_root_path(&conn, &repo_root.to_string_lossy())
                .unwrap()
                .map(|id| id.to_string()),
            Some(repo_id.to_string()),
        );
    }

    #[test]
    fn guarded_step_removes_repository_when_root_stays_inactive() {
        let conn = migrated_conn();
        let repo_root = PathBuf::from(r"C:\workspace\repo");
        let repo_id = attic_core::RepositoryId::new_v4();
        upsert_repository(
            &conn,
            &repo_id,
            &repo_root.to_string_lossy(),
            "repo-removed",
        )
        .unwrap();
        sync_workspace_membership(&conn, &[]).unwrap();

        let step = evict_repository_step_if_inactive(&conn, &repo_id.to_string(), &repo_root)
            .unwrap()
            .expect("inactive root must run the delete step");
        assert!(
            step.complete,
            "repository-only row should delete in one step"
        );
        assert!(
            lookup_repository_by_root_path(&conn, &repo_root.to_string_lossy())
                .unwrap()
                .is_none(),
            "the removed repository row must be gone after eviction"
        );
    }
}
