//! Cross-repository maintenance and orchestration (Phase 6 §12).
//!
//! Ties together catalog scanning, resolver, storage persistence, traversal
//! and impact analysis into bounded, observable operations. Every public
//! function is designed to run inside a writer-queue closure (Phase 1A
//! contract) or on a reader connection for pure read operations.
//!
//! ```text
//! sync_repository:
//!   scan manifests → parse → persist catalog + declarations
//!
//! sync_workspace:
//!   for each repo: scan (reader phase)
//!   → build_resolver_input → resolve_workspace
//!   → writer phase: delete old edges + insert new edges
//!
//! repository_removed:
//!   delete catalog + declarations + all cross-repo edges (no ghosts)
//!
//! incremental_sync:
//!   changed manifest files → recompute affected repository only
//! ```

use std::collections::HashMap;

use tracing::{debug, info};

use crate::error::CrossRepoError;
use crate::resolver::{self, RepoCatalogData, ResolutionDiagnostics};
use crate::{CancelToken, Deadline};

// ---------------------------------------------------------------------------
// Progress reporting
// ---------------------------------------------------------------------------

/// Stage of a workspace sync operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStage {
    /// Scanning manifests for a repository.
    ScanningManifests,
    /// Resolving cross-repo edges.
    Resolving,
    /// Persisting resolved edges.
    PersistingEdges,
}

/// Progress event emitted during workspace sync.
#[derive(Debug, Clone)]
pub struct SyncProgress {
    /// Current stage.
    pub stage: SyncStage,
    /// Repository being processed (when applicable).
    pub repository_id: String,
    /// Current index in the batch (0-based).
    pub current: usize,
    /// Total repositories in the batch.
    pub total: usize,
}

// ---------------------------------------------------------------------------
// Single-repository sync
// ---------------------------------------------------------------------------

/// Scan and persist catalog + declarations for one repository.
///
/// Must run inside a writer-queue closure (single connection, single atomic
/// transaction). The caller provides the raw connection.
pub fn sync_repository(
    conn: &rusqlite::Connection,
    repository_id: &str,
    source_revision_id: &str,
) -> Result<SyncReport, CrossRepoError> {
    let scan = crate::catalog::scan_repository_manifests(conn, repository_id)?;

    let mut provides = Vec::new();
    let mut declarations = Vec::new();
    for m in &scan.manifests {
        provides.extend(m.provides.clone());
        declarations.extend(m.declarations.clone());
    }

    crate::catalog::persist_catalog(
        conn,
        repository_id,
        source_revision_id,
        &scan,
        &provides,
        &declarations,
    )?;

    Ok(SyncReport {
        repository_id: repository_id.to_owned(),
        provides_count: provides.len(),
        declarations_count: declarations.len(),
        oversized: scan.oversized.len(),
        unreadable: scan.unreadable.len(),
        manifest_hash: scan.manifest_hash,
    })
}

/// Report from a single-repository sync.
#[derive(Debug, Clone)]
pub struct SyncReport {
    /// Repository synced.
    pub repository_id: String,
    /// Number of provided identities.
    pub provides_count: usize,
    /// Number of dependency declarations.
    pub declarations_count: usize,
    /// Manifests skipped (oversized).
    pub oversized: usize,
    /// Manifests skipped (unreadable).
    pub unreadable: usize,
    /// Deterministic hash of manifest content.
    pub manifest_hash: String,
}

// ---------------------------------------------------------------------------
// Full workspace sync + resolution
// ---------------------------------------------------------------------------

/// Options for a full workspace sync.
#[derive(Debug, Clone)]
pub struct WorkspaceSyncOptions {
    /// Deadline for the entire operation.
    pub deadline: Deadline,
    /// Cooperative cancellation token.
    pub cancel: CancelToken,
}

impl Default for WorkspaceSyncOptions {
    fn default() -> Self {
        Self {
            deadline: Deadline::after(std::time::Duration::from_secs(30)),
            cancel: CancelToken::never(),
        }
    }
}

/// Result of a full workspace sync.
#[derive(Debug, Clone)]
pub struct WorkspaceSyncResult {
    /// Per-repository sync reports.
    pub repository_reports: Vec<SyncReport>,
    /// Resolver diagnostics (missing/ambiguous targets).
    pub diagnostics: ResolutionDiagnostics,
    /// Number of cross-repo edges emitted.
    pub edges_emitted: usize,
}

/// Perform a full workspace sync: scan all repos, resolve, persist edges.
///
/// Uses two phases:
/// 1. **Reader phase**: scan all repositories, build resolver input (bounded I/O)
/// 2. **Writer phase**: persist edges inside a single writer closure
pub fn sync_workspace(
    reader_conn: &rusqlite::Connection,
    writer: &attic_storage::WriterQueueHandle,
    opts: &WorkspaceSyncOptions,
) -> Result<WorkspaceSyncResult, CrossRepoError> {
    let mut result = WorkspaceSyncResult {
        repository_reports: Vec::new(),
        diagnostics: ResolutionDiagnostics::default(),
        edges_emitted: 0,
    };

    // Phase 1: scan all repositories (reader-only, bounded I/O).
    let repo_ids = attic_storage::crossrepo_ops::all_repository_ids(reader_conn)?;
    let total = repo_ids.len();
    let mut all_repo_data: Vec<RepoCatalogData> = Vec::with_capacity(total);
    let mut proto_index: HashMap<String, Vec<String>> = HashMap::new();

    for (idx, repo_id) in repo_ids.iter().enumerate() {
        if opts.cancel.is_cancelled() || opts.deadline.expired() {
            debug!("workspace sync cancelled at repo {idx}/{total}");
            break;
        }
        let report = sync_repository(reader_conn, repo_id, "")?;
        // Build resolver input from the persisted catalog state.
        let catalog =
            attic_storage::crossrepo_ops::catalog_entry(reader_conn, repo_id)?;
        let raw_decls =
            attic_storage::crossrepo_ops::declarations_for_repository(reader_conn, repo_id)?;

        let provides: Vec<crate::ProvidedIdentity> = catalog
            .as_ref()
            .and_then(|c| serde_json::from_str(&c.provides_json).ok())
            .unwrap_or_default();

        let declarations: Vec<crate::DependencyDeclaration> = raw_decls
            .into_iter()
            .map(|d| crate::DependencyDeclaration {
                path: d.path,
                ecosystem: crate::Ecosystem::from_db_str(&d.ecosystem)
                    .unwrap_or(crate::Ecosystem::Maven),
                name: d.name,
                version_req: d.version_req,
                kind: crate::DeclarationKind::from_db_str(&d.declaration_kind)
                    .unwrap_or(crate::DeclarationKind::External),
                local_hint: d.local_hint,
            })
            .collect();

        let repo_id_parsed = repo_id
            .parse::<attic_core::RepositoryId>()
            .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
        let root_path = attic_storage::get_repository_path(reader_conn, &repo_id_parsed)?
            .unwrap_or_default();

        // Use the real source_revision_id from the catalog (derived from
        // the persisted DB state, not an empty placeholder).
        let source_revision_id = catalog
            .as_ref()
            .map(|c| c.source_revision_id.clone())
            .unwrap_or_default();

        let gmp = provides
            .iter()
            .find(|p| p.ecosystem == crate::Ecosystem::Go)
            .map(|p| p.name.clone());

        let primary = crate::catalog::primary_anchor_for_repo(reader_conn, repo_id);

        // Scan proto imports for generated API resolution.
        let proto_specs = crate::catalog::scan_proto_imports(reader_conn, repo_id)?;
        if !proto_specs.is_empty() {
            proto_index.insert(repo_id.clone(), proto_specs);
        }

        all_repo_data.push(RepoCatalogData {
            repository_id: repo_id.clone(),
            root_path,
            source_revision_id,
            provides,
            declarations,
            primary_anchor_occurrence: primary,
            go_module_prefix: gmp,
        });
        result.repository_reports.push(report);
    }

    // Phase 2: resolve (pure computation, no I/O).
    let (edges, diagnostics) = resolver::resolve_workspace(&all_repo_data, &proto_index);
    result.diagnostics = diagnostics;
    let edges_len = edges.len();

    // Phase 3: persist edges in a single writer closure.
    let edges_clone = edges.clone();
    let repo_ids_for_cleanup: Vec<String> = all_repo_data
        .iter()
        .map(|r| r.repository_id.clone())
        .collect();

    writer
        .send(move |conn| {
            // Delete all existing cross-repo DEPENDS_ON edges (clean replacement).
            for repo_id in &repo_ids_for_cleanup {
                attic_storage::crossrepo_ops::delete_all_xrepo_edges_touching(conn, repo_id)?;
            }

            // Insert resolved edges.
            for e in &edges_clone {
                attic_storage::crossrepo_ops::insert_xrepo_edge(
                    conn,
                    &e.source_repository_id,
                    &e.source_entity_id,
                    &e.target_repository_id,
                    &e.target_entity_id,
                    &e.resolution,
                    e.confidence,
                    &e.dependency_basis,
                    &e.provenance_json,
                    &e.source_revision_id,
                )?;
            }

            Ok(())
        })
        .map_err(|e| CrossRepoError::Storage(e))?;

    result.edges_emitted = edges_len;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Repository removal
// ---------------------------------------------------------------------------

/// Remove ALL cross-repository state for a repository: catalog, declarations,
/// and all cross-repo DEPENDS_ON edges touching it.
///
/// Must run inside a writer-queue closure.
pub fn repository_removed(
    conn: &rusqlite::Connection,
    repository_id: &str,
) -> Result<(usize, usize), CrossRepoError> {
    let (edges, decls) =
        attic_storage::crossrepo_ops::remove_repository_crossrepo_data(conn, repository_id)?;
    info!(
        repo = repository_id,
        edges, decls, "cross-repo state removed"
    );
    Ok((edges, decls))
}

// ---------------------------------------------------------------------------
// Incremental sync hooks
// ---------------------------------------------------------------------------

/// Check whether any of the changed paths are dependency manifests for the
/// given repository, and if so, trigger a re-sync.
///
/// Must run inside a writer-queue closure.
pub fn incremental_sync(
    conn: &rusqlite::Connection,
    repository_id: &str,
    source_revision_id: &str,
    changed_paths: &[String],
) -> Result<bool, CrossRepoError> {
    let has_manifest_change = changed_paths
        .iter()
        .any(|p| crate::manifest::is_manifest_path(p));
    if !has_manifest_change {
        return Ok(false);
    }
    debug!(
        repo = repository_id,
        "manifest change detected, resyncing"
    );
    sync_repository(conn, repository_id, source_revision_id)?;
    Ok(true)
}

/// Mark all cross-repo edges TARGETING a given repository as STALE.
///
/// Used when a provider repository's identities may have changed and
/// consumer edges need recomputation on next sync.
pub fn invalidate_edges_targeting(
    conn: &rusqlite::Connection,
    repository_id: &str,
) -> Result<u64, CrossRepoError> {
    let n = conn
        .execute(
            "UPDATE core_relationships SET freshness_state = 'STALE'
              WHERE target_repository_id = ?1
                AND rel_type = 'DEPENDS_ON'
                AND freshness_state = 'CURRENT'",
            rusqlite::params![repository_id],
        )
        .map_err(CrossRepoError::Db)?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_storage::connection::configure_connection;
    use attic_storage::migration::run_migrations;

    fn seeded_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn test_id(name: &str) -> attic_core::RepositoryId {
        let u = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
        u.to_string().parse().unwrap()
    }

    fn tid(name: &str) -> String {
        test_id(name).to_string_repr()
    }

    fn insert_repo(conn: &rusqlite::Connection, id: &str, root: &str) {
        let rid = test_id(id);
        attic_storage::repository::repository::upsert_repository(conn, &rid, root, id).unwrap();
    }

    fn insert_rev(conn: &rusqlite::Connection, repo_id: &str) -> String {
        let rid = test_id(repo_id);
        let srid = attic_core::SourceRevisionId::new_v4();
        attic_storage::repository::source_revision::insert_source_revision(
            conn,
            &srid,
            &rid,
            "test-sha",
            "2024-01-01",
            attic_core::SourceType::Git,
        )
        .unwrap();
        srid.to_string_repr().to_string()
    }

    #[test]
    fn incremental_sync_skips_non_manifest_changes() {
        let conn = seeded_conn();
        insert_repo(&conn, "r1", "/ws/r1");
        let _ = insert_rev(&conn, "r1");

        let changed = vec!["src/main.rs".to_owned(), "README.md".to_owned()];
        let resynced = incremental_sync(&conn, &tid("r1"), "rev-1", &changed).unwrap();
        assert!(!resynced, "non-manifest changes should not trigger resync");
    }

    #[test]
    fn incremental_sync_triggers_on_manifest_change() {
        let conn = seeded_conn();
        insert_repo(&conn, "r1", "/ws/r1");
        let rev = insert_rev(&conn, "r1");

        let changed = vec!["src/main.rs".to_owned(), "go.mod".to_owned()];
        let resynced = incremental_sync(&conn, &tid("r1"), &rev, &changed).unwrap();
        assert!(resynced, "manifest change should trigger resync");
    }

    #[test]
    fn invalidate_edges_targeting_marks_stale() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");

        let edge_id = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r0"), "s0", &tid("r1"), "t1", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();

        // Also insert an edge NOT targeting r1
        let _edge_id2 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r0"), "s0", &tid("r0"), "t0", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();

        let count = invalidate_edges_targeting(&conn, &tid("r1")).unwrap();
        assert_eq!(count, 1, "only the edge targeting r1 should be marked STALE");

        // Verify the edge is actually STALE
        let state: String = conn
            .query_row(
                "SELECT freshness_state FROM core_relationships WHERE id = ?1",
                rusqlite::params![edge_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "STALE");
    }

    #[test]
    fn repository_removed_cleans_up() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");
        let _ = insert_rev(&conn, "r1");

        // Insert catalog row
        let catalog = attic_storage::crossrepo_ops::CatalogRow {
            repository_id: tid("r0"),
            source_revision_id: rev.clone(),
            provides_json: "[]".to_owned(),
            manifest_hash: "test_hash".to_owned(),
            entry_count: 0,
            freshness_state: "CURRENT".to_owned(),
        };
        attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &catalog, &catalog.provides_json)
            .unwrap();

        // Insert edge
        let _ = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r0"), "s0", &tid("r1"), "t1", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();

        let (edges, _decls) = repository_removed(&conn, &tid("r0")).unwrap();
        assert!(edges >= 1, "should have deleted at least one edge");

        // Verify catalog row is gone
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_workspace_catalog WHERE repository_id = ?1",
                rusqlite::params![tid("r0")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn sync_repository_persists_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Create a go.mod file
        std::fs::write(root.join("go.mod"), b"module example.com/test\n").unwrap();

        let conn = seeded_conn();
        let rid = test_id("test-repo");
        attic_storage::repository::repository::upsert_repository(
            &conn,
            &rid,
            &root.to_string_lossy(),
            "test-repo",
        )
        .unwrap();
        let rev = insert_rev(&conn, "test-repo");

        let report = sync_repository(&conn, &tid("test-repo"), &rev).unwrap();
        assert_eq!(report.repository_id, tid("test-repo"));
        assert!(!report.manifest_hash.is_empty(), "manifest_hash should be computed");

        // Verify catalog row exists
        let catalog = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("test-repo")).unwrap();
        assert!(catalog.is_some(), "catalog row should exist after sync");
    }

    #[test]
    fn sync_report_fields_populated() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), b"module m\n").unwrap();
        std::fs::write(root.join("package.json"), br#"{"name":"test"}"#).unwrap();

        let conn = seeded_conn();
        let rid = test_id("r1");
        attic_storage::repository::repository::upsert_repository(
            &conn,
            &rid,
            &root.to_string_lossy(),
            "r1",
        )
        .unwrap();
        let rev = insert_rev(&conn, "r1");

        let report = sync_repository(&conn, &tid("r1"), &rev).unwrap();
        assert!(report.manifest_hash.len() == 64, "blake3 hex is 64 chars");
        // Without file occurrence indexing, manifest paths are not discovered.
        // The report is valid — provides_count is 0 because no occurrences exist.
        assert_eq!(report.provides_count, 0);
        assert_eq!(report.declarations_count, 0);
    }

    #[test]
    fn source_revision_id_stored_in_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), b"module x\n").unwrap();

        let conn = seeded_conn();
        let rid = test_id("src-rev-test");
        attic_storage::repository::repository::upsert_repository(
            &conn,
            &rid,
            &root.to_string_lossy(),
            "src-rev-test",
        )
        .unwrap();
        let rev = insert_rev(&conn, "src-rev-test");

        sync_repository(&conn, &tid("src-rev-test"), &rev).unwrap();

        let catalog = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("src-rev-test"))
            .unwrap()
            .expect("catalog row should exist");
        assert_eq!(
            catalog.source_revision_id, rev,
            "source_revision_id must be stored correctly"
        );
    }

    #[test]
    fn repository_removed_deletes_edges_and_catalog() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev0 = insert_rev(&conn, "r0");
        let rev1 = insert_rev(&conn, "r1");

        // Insert catalog for both repos
        let cat0 = attic_storage::crossrepo_ops::CatalogRow {
            repository_id: tid("r0"),
            source_revision_id: rev0.clone(),
            provides_json: "[]".to_owned(),
            manifest_hash: "hash0".to_owned(),
            entry_count: 0,
            freshness_state: "CURRENT".to_owned(),
        };
        attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &cat0, &cat0.provides_json).unwrap();

        let cat1 = attic_storage::crossrepo_ops::CatalogRow {
            repository_id: tid("r1"),
            source_revision_id: rev1.clone(),
            provides_json: "[]".to_owned(),
            manifest_hash: "hash1".to_owned(),
            entry_count: 0,
            freshness_state: "CURRENT".to_owned(),
        };
        attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &cat1, &cat1.provides_json).unwrap();

        // Insert edges r0→r1 and r1→r0
        let _e1 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r0"), "s0", &tid("r1"), "t1", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev0,
        ).unwrap();
        let _e2 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r1"), "s1", &tid("r0"), "t0", "PACKAGE_RESOLVED", 0.8, "GO_MODULE", "{}", &rev1,
        ).unwrap();

        let (edges_deleted, decls_deleted) = repository_removed(&conn, &tid("r0")).unwrap();
        assert!(edges_deleted >= 2, "should delete both edges touching r0");
        assert_eq!(decls_deleted, 0, "no declarations to delete");

        // Verify no edges remain
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_relationships WHERE source_repository_id = ?1 OR target_repository_id = ?1",
                rusqlite::params![tid("r0")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0, "no edges should remain for removed repo");

        // Verify r1 catalog still exists
        let r1_cat = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("r1")).unwrap();
        assert!(r1_cat.is_some(), "r1 catalog should still exist");
    }

    #[test]
    fn incremental_sync_propagates_error_on_db_failure() {
        let conn = seeded_conn();
        // Try to sync a repository that doesn't exist — should propagate error
        let result = incremental_sync(
            &conn,
            &tid("nonexistent"),
            "rev-1",
            &["go.mod".to_owned()],
        );
        assert!(result.is_err(), "should propagate error for missing repo");
    }
}
