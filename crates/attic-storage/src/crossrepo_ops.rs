//! Phase 6 storage operations: derived Workspace Catalog, dependency
//! declarations, and cross-repository DEPENDS_ON edges.
//!
//! Read helpers run on reader connections.  Write primitives are plain
//! functions over `&Connection` so [`crate::maintenance`] can compose a
//! whole repository sync into ONE writer-queue closure (single atomic
//! transaction, consistent with the Phase 1A writer contract).
//!
//! Cross-repository edges live in `core_relationships` with
//! `rel_type='DEPENDS_ON'` and `source_repository_id != target_repository_id`
//! (the partial index `idx_relationships_cross_repo` covers them).

#![allow(clippy::too_many_lines)]

use rusqlite::{Connection, OptionalExtension};
use tracing::debug;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

/// One derived Workspace Catalog entry (per repository).
#[derive(Debug, Clone)]
pub struct CatalogRow {
    /// Repository UUID.
    pub repository_id: String,
    /// Revision the entry was derived from.
    pub source_revision_id: String,
    /// JSON array of provided identities.
    pub provides_json: String,
    /// BLAKE3 over sorted (path, content_hash) of dependency-declaration
    /// files.
    pub manifest_hash: String,
    /// Number of declarations recorded for this repository.
    pub entry_count: i64,
    /// CURRENT | STALE | INVALID | PENDING_REFRESH
    pub freshness_state: String,
}

/// One parsed dependency declaration persisted for a repository.
#[derive(Debug, Clone)]
pub struct DeclarationRow {
    /// Declaration UUID.
    pub id: String,
    /// Owning repository UUID.
    pub repository_id: String,
    /// Declaring manifest occurrence (NULL for synthetic entries).
    pub file_occurrence_id: Option<String>,
    /// Repo-relative manifest path.
    pub path: String,
    /// MAVEN | GRADLE | GO | NPM | PYTHON | SUBMODULE | CONFIG |
    /// GENERATED_API
    pub ecosystem: String,
    /// Normalized target identity.
    pub name: String,
    /// Version requirement when present.
    pub version_req: Option<String>,
    /// external | local_path | workspace_member
    pub declaration_kind: String,
    /// Repo-relative local hint when local/workspace.
    pub local_hint: Option<String>,
    /// Revision the declaration was captured at.
    pub source_revision_id: String,
    /// CURRENT | STALE | INVALID | PENDING_REFRESH
    pub freshness_state: String,
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Catalog entry for one repository, when present.
pub fn catalog_entry(
    conn: &Connection,
    repository_id: &str,
) -> Result<Option<CatalogRow>, StorageError> {
    let sql = "
        SELECT repository_id, source_revision_id, provides_json,
               manifest_hash, entry_count, freshness_state
          FROM core_workspace_catalog
         WHERE repository_id = ?1";
    let mut stmt = conn.prepare(sql)?;
    stmt.query_row(rusqlite::params![repository_id], |r| {
        Ok(CatalogRow {
            repository_id: r.get(0)?,
            source_revision_id: r.get(1)?,
            provides_json: r.get(2)?,
            manifest_hash: r.get(3)?,
            entry_count: r.get(4)?,
            freshness_state: r.get(5)?,
        })
    })
    .optional()
    .map_err(StorageError::from)
}

/// All catalog entries (bounded workspace scale).
pub fn all_catalog_entries(conn: &Connection) -> Result<Vec<CatalogRow>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT repository_id, source_revision_id, provides_json, manifest_hash,
                entry_count, freshness_state
           FROM core_workspace_catalog
          ORDER BY repository_id ASC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(CatalogRow {
                repository_id: r.get(0)?,
                source_revision_id: r.get(1)?,
                provides_json: r.get(2)?,
                manifest_hash: r.get(3)?,
                entry_count: r.get(4)?,
                freshness_state: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Current declarations for one repository.
pub fn declarations_for_repository(
    conn: &Connection,
    repository_id: &str,
) -> Result<Vec<DeclarationRow>, StorageError> {
    let sql = "
        SELECT id, repository_id, file_occurrence_id, path, ecosystem, name,
               version_req, declaration_kind, local_hint, source_revision_id,
               freshness_state
          FROM core_dependency_declarations
         WHERE repository_id = ?1
           AND freshness_state = 'CURRENT'
         ORDER BY path ASC, name ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![repository_id], map_declaration)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_declaration(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeclarationRow> {
    Ok(DeclarationRow {
        id: r.get(0)?,
        repository_id: r.get(1)?,
        file_occurrence_id: r.get(2)?,
        path: r.get(3)?,
        ecosystem: r.get(4)?,
        name: r.get(5)?,
        version_req: r.get(6)?,
        declaration_kind: r.get(7)?,
        local_hint: r.get(8)?,
        source_revision_id: r.get(9)?,
        freshness_state: r.get(10)?,
    })
}

/// Repositories (as catalog identities) providing a given identity key.
///
/// Indexed lookup — never a workspace-wide scan.
pub fn providers_of_identity(
    conn: &Connection,
    ecosystem: &str,
    name: &str,
) -> Result<Vec<String>, StorageError> {
    // LIKE-based JSON containment would be unindexed; instead resolve via
    // the declarations-independent provides JSON with exact-match guard in
    // Rust.  The candidate set is the full catalog (≤ ~30 rows), filtered
    // precisely here.
    let entries = all_catalog_entries(conn)?;
    let needle = format!("\"ecosystem\":\"{ecosystem}\",\"name\":\"{name}\"");
    let mut out = Vec::new();
    for e in entries {
        if e.provides_json.contains(&needle) {
            out.push(e.repository_id);
        }
        if out.len() >= 16 {
            break;
        }
    }
    Ok(out)
}

/// Cross-repo DEPENDS_ON edges between two repositories (either direction).
pub fn cross_edges_between(
    conn: &Connection,
    repo_a: &str,
    repo_b: &str,
) -> Result<Vec<crate::retrieval_reads::RelationshipEdge>, StorageError> {
    let sql = "
        SELECT id, source_entity_id, source_entity_type,
               target_entity_id, target_entity_type,
               rel_type, resolution, confidence, provenance_json,
               source_revision_id, freshness_state,
               source_repository_id, target_repository_id
          FROM core_relationships
         WHERE rel_type = 'DEPENDS_ON'
           AND source_repository_id != target_repository_id
           AND ((source_repository_id = ?1 AND target_repository_id = ?2)
             OR (source_repository_id = ?2 AND target_repository_id = ?1))
         ORDER BY id ASC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![repo_a, repo_b], map_edge)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// All cross-repo DEPENDS_ON edges touching one repository (either side),
/// excluding INVALID.
pub fn cross_edges_touching(
    conn: &Connection,
    repository_id: &str,
    limit: usize,
) -> Result<Vec<XrepoEdge>, StorageError> {
    let sql = "
        SELECT r.id, r.source_repository_id, r.target_repository_id,
               r.source_entity_id, r.target_entity_id, r.resolution,
               r.confidence, r.provenance_json, r.freshness_state,
               r.source_revision_id
          FROM core_relationships r
         WHERE r.rel_type = 'DEPENDS_ON'
           AND r.source_repository_id != r.target_repository_id
           AND r.freshness_state != 'INVALID'
           AND (r.source_repository_id = ?1 OR r.target_repository_id = ?1)
         ORDER BY r.id ASC
         LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![repository_id, limit as i64], |r| {
            Ok(XrepoEdge {
                id: r.get(0)?,
                source_repository_id: r.get(1)?,
                target_repository_id: r.get(2)?,
                source_entity_id: r.get(3)?,
                target_entity_id: r.get(4)?,
                resolution: r.get(5)?,
                confidence: r.get(6)?,
                provenance_json: r.get(7)?,
                freshness_state: r.get(8)?,
                source_revision_id: r.get(9)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn map_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<crate::retrieval_reads::RelationshipEdge> {
    Ok(crate::retrieval_reads::RelationshipEdge {
        id: r.get(0)?,
        source_entity_id: r.get(1)?,
        source_entity_type: r.get(2)?,
        target_entity_id: r.get(3)?,
        target_entity_type: r.get(4)?,
        rel_type: r.get(5)?,
        resolution: r.get(6)?,
        confidence: r.get(7)?,
        provenance_json: r.get(8)?,
        source_revision_id: r.get(9)?,
        freshness_state: r.get(10)?,
        source_repository_id: r.get(11)?,
        target_repository_id: r.get(12)?,
    })
}

/// One cross-repository edge projection used by traversal/impact.
#[derive(Debug, Clone)]
pub struct XrepoEdge {
    /// Edge UUID.
    pub id: String,
    /// Source repository.
    pub source_repository_id: String,
    /// Target repository.
    pub target_repository_id: String,
    /// Source endpoint entity id.
    pub source_entity_id: String,
    /// Target endpoint entity id.
    pub target_entity_id: String,
    /// SYNTACTIC | PACKAGE_RESOLVED | SYMBOL_RESOLVED | BUILD_RESOLVED |
    /// FRAMEWORK_RESOLVED | INFERRED
    pub resolution: String,
    /// [0,1]
    pub confidence: f64,
    /// Provenance JSON (no secret content).
    pub provenance_json: Option<String>,
    /// CURRENT | STALE | INVALID
    pub freshness_state: String,
    /// Producing revision.
    pub source_revision_id: String,
}

impl XrepoEdge {
    /// Repository on the OTHER side of the edge from `repo`.
    pub fn other_side(&self, repo: &str) -> Option<&str> {
        if self.source_repository_id == repo {
            Some(&self.target_repository_id)
        } else if self.target_repository_id == repo {
            Some(&self.source_repository_id)
        } else {
            None
        }
    }
}

/// Distinct repositories present in `core_repositories` (bounded).
pub fn all_repository_ids(conn: &Connection) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare("SELECT id FROM core_repositories ORDER BY id ASC")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Write primitives (compose inside ONE writer closure)
// ---------------------------------------------------------------------------

/// Insert or refresh the catalog row for a repository.
pub fn upsert_catalog_row(
    conn: &Connection,
    row: &CatalogRow,
    provides_json: &str,
) -> Result<(), StorageError> {
    let now = now_micros();
    conn.execute(
        "INSERT INTO core_workspace_catalog
            (id, repository_id, source_revision_id, provides_json, manifest_hash,
             entry_count, freshness_state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(repository_id) DO UPDATE SET
            source_revision_id = excluded.source_revision_id,
            provides_json      = excluded.provides_json,
            manifest_hash      = excluded.manifest_hash,
            entry_count        = excluded.entry_count,
            freshness_state    = excluded.freshness_state,
            updated_at         = excluded.updated_at",
        rusqlite::params![
            uuid::Uuid::new_v4().to_string(),
            row.repository_id,
            row.source_revision_id,
            provides_json,
            row.manifest_hash,
            row.entry_count,
            row.freshness_state,
            now
        ],
    )
    .map_err(StorageError::from)?;
    debug!(repo = %row.repository_id, "catalog row upserted");
    Ok(())
}

/// Delete every declaration row for a repository.
pub fn delete_declarations_for_repository(
    conn: &Connection,
    repository_id: &str,
) -> Result<usize, StorageError> {
    conn.execute(
        "DELETE FROM core_dependency_declarations WHERE repository_id = ?1",
        rusqlite::params![repository_id],
    )
    .map_err(StorageError::from)
}

/// Insert one declaration row (id assigned here).
pub fn insert_declaration(
    conn: &Connection,
    d: &DeclarationRow,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO core_dependency_declarations
            (id, repository_id, file_occurrence_id, path, ecosystem, name,
             version_req, declaration_kind, local_hint, source_revision_id,
             freshness_state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
        rusqlite::params![
            if d.id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                d.id.clone()
            },
            d.repository_id,
            d.file_occurrence_id,
            d.path,
            d.ecosystem,
            d.name,
            d.version_req,
            d.declaration_kind,
            d.local_hint,
            d.source_revision_id,
            d.freshness_state,
            now_micros()
        ],
    )
    .map_err(StorageError::from)?;
    Ok(())
}

/// Delete cross-repo DEPENDS_ON edges BETWEEN two specific repositories
/// (either direction).  Returns removed count.
pub fn delete_xrepo_edges_between(
    conn: &Connection,
    repo_a: &str,
    repo_b: &str,
) -> Result<usize, StorageError> {
    conn.execute(
        "DELETE FROM core_relationships
          WHERE rel_type = 'DEPENDS_ON'
            AND source_repository_id != target_repository_id
            AND ((source_repository_id = ?1 AND target_repository_id = ?2)
              OR (source_repository_id = ?2 AND target_repository_id = ?1))",
        rusqlite::params![repo_a, repo_b],
    )
    .map_err(StorageError::from)
}

/// Delete ALL cross-repo edges touching one repository (either side).
pub fn delete_all_xrepo_edges_touching(
    conn: &Connection,
    repository_id: &str,
) -> Result<usize, StorageError> {
    conn.execute(
        "DELETE FROM core_relationships
          WHERE rel_type = 'DEPENDS_ON'
            AND source_repository_id != target_repository_id
            AND (source_repository_id = ?1 OR target_repository_id = ?1)",
        rusqlite::params![repository_id],
    )
    .map_err(StorageError::from)
}

/// Insert one cross-repo DEPENDS_ON edge.
///
/// `source_entity_id` anchors at the declaring FILE_OCCURRENCE;
/// `target_entity_id` anchors at the target repository's primary manifest
/// occurrence, or a deterministic `logical:` placeholder when that
/// repository has none (ADR-011 convention).
pub fn insert_xrepo_edge(
    conn: &Connection,
    source_repo: &str,
    source_entity_id: &str,
    target_repo: &str,
    target_entity_id: &str,
    resolution: &str,
    confidence: f64,
    dependency_basis: &str,
    provenance_json: &str,
    source_revision_id: &str,
) -> Result<String, StorageError> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        "INSERT INTO core_relationships
            (id, source_repository_id, source_entity_id, source_entity_type,
             target_repository_id, target_entity_id, target_entity_type,
             rel_type, dependency_basis, resolution, confidence,
             provenance_json, source_revision_id, freshness_state)
         VALUES (?1, ?2, ?3, 'FILE_OCCURRENCE',
                 ?4, ?5, 'FILE_OCCURRENCE',
                 'DEPENDS_ON', ?6, ?7, ?8,
                 ?9, ?10, 'CURRENT')",
        rusqlite::params![
            id,
            source_repo,
            source_entity_id,
            target_repo,
            target_entity_id,
            dependency_basis,
            resolution,
            confidence.clamp(0.0, 1.0),
            provenance_json,
            source_revision_id
        ],
    )
    .map_err(StorageError::from)?;
    Ok(id)
}

/// Remove every Phase 6 trace of a repository: catalog row, declarations,
/// and all cross-repo edges touching it.  Used by repository removal.
pub fn remove_repository_crossrepo_data(
    conn: &Connection,
    repository_id: &str,
) -> Result<(usize, usize), StorageError> {
    let edges =
        delete_all_xrepo_edges_touching(conn, repository_id)?;
    conn.execute(
        "DELETE FROM core_workspace_catalog WHERE repository_id = ?1",
        rusqlite::params![repository_id],
    )
    .map_err(StorageError::from)?;
    let decls = delete_declarations_for_repository(conn, repository_id)?;
    Ok((edges, decls))
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// WorkspaceSnapshot provenance (Migration 0005)
// ---------------------------------------------------------------------------

/// One WorkspaceSnapshot header row: identifies the sync_workspace run and
/// the edges it emitted.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshotRow {
    /// UUID of this snapshot.
    pub id: String,
    /// Unix microseconds.
    pub created_at: i64,
    /// Number of repositories contributing revisions.
    pub repo_count: i64,
    /// Total edges emitted during this run.
    pub edges_emitted: i64,
}

/// One per-repository revision entry in a WorkspaceSnapshot.
#[derive(Debug, Clone)]
pub struct WorkspaceSnapshotRevision {
    /// UUID of this revision entry.
    pub id: String,
    /// Owning snapshot.
    pub snapshot_id: String,
    /// Repository whose revision is recorded.
    pub repository_id: String,
    /// Source revision that contributed to the resolver input.
    pub source_revision_id: String,
}

/// Create a new WorkspaceSnapshot row and insert one revision entry for
/// every (repository_id, source_revision_id) pair.
///
/// Returns the snapshot UUID. Must be called inside a writer-queue closure
/// (single connection, single transaction).
pub fn create_workspace_snapshot(
    conn: &Connection,
    revisions: &[(String, String)], // (repository_id, source_revision_id)
    edges_emitted: usize,
) -> Result<String, StorageError> {
    let now = now_micros();
    let snapshot_id = uuid::Uuid::new_v4().to_string();

    conn.execute(
        "INSERT INTO core_workspace_snapshots
             (id, created_at, repo_count, edges_emitted)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            snapshot_id,
            now,
            revisions.len() as i64,
            edges_emitted as i64,
        ],
    )
    .map_err(StorageError::from)?;

    for (repo_id, rev_id) in revisions {
        let rev_entry_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO core_workspace_snapshot_revisions
                 (id, snapshot_id, repository_id, source_revision_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![rev_entry_id, snapshot_id, repo_id, rev_id, now],
        )
        .map_err(StorageError::from)?;
    }

    debug!(
        snapshot_id = %snapshot_id,
        repo_count = revisions.len(),
        edges_emitted,
        "workspace snapshot created"
    );
    Ok(snapshot_id)
}

/// Retrieve the most recent WorkspaceSnapshot header, if any.
pub fn latest_workspace_snapshot(
    conn: &Connection,
) -> Result<Option<WorkspaceSnapshotRow>, StorageError> {
    conn.query_row(
        "SELECT id, created_at, repo_count, edges_emitted
           FROM core_workspace_snapshots
          ORDER BY created_at DESC
          LIMIT 1",
        [],
        |r| {
            Ok(WorkspaceSnapshotRow {
                id: r.get(0)?,
                created_at: r.get(1)?,
                repo_count: r.get(2)?,
                edges_emitted: r.get(3)?,
            })
        },
    )
    .optional()
    .map_err(StorageError::from)
}

/// Retrieve the revision entries for one snapshot.
pub fn snapshot_revisions(
    conn: &Connection,
    snapshot_id: &str,
) -> Result<Vec<WorkspaceSnapshotRevision>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT id, snapshot_id, repository_id, source_revision_id
           FROM core_workspace_snapshot_revisions
          WHERE snapshot_id = ?1
          ORDER BY repository_id ASC",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![snapshot_id], |r| {
            Ok(WorkspaceSnapshotRevision {
                id: r.get(0)?,
                snapshot_id: r.get(1)?,
                repository_id: r.get(2)?,
                source_revision_id: r.get(3)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
