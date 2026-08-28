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
///
/// Looks up the latest source revision from the DB; never accepts an
/// empty or placeholder source revision.
pub fn sync_repository(
    conn: &rusqlite::Connection,
    repository_id: &str,
) -> Result<SyncReport, CrossRepoError> {
    let scan = crate::catalog::scan_repository_manifests(conn, repository_id)?;

    let mut provides = Vec::new();
    let mut declarations = Vec::new();
    for m in &scan.manifests {
        provides.extend(m.provides.clone());
        declarations.extend(m.declarations.clone());
    }

    // Resolve the actual source revision from the authoritative DB state.
    let source_revision_id = resolve_source_revision(conn, repository_id)?;

    crate::catalog::persist_catalog(
        conn,
        repository_id,
        &source_revision_id,
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

/// Resolve the latest source revision ID for a repository from the DB.
///
/// Returns the `id` column of the most recently captured `core_source_revisions`
/// row. Fails with `NoSourceRevision` if the repository has not been indexed.
fn resolve_source_revision(
    conn: &rusqlite::Connection,
    repository_id: &str,
) -> Result<String, CrossRepoError> {
    let repo_id = repository_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
    attic_storage::latest_source_revision_for_repository(conn, &repo_id)?.ok_or_else(|| {
        CrossRepoError::NoSourceRevision {
            repository_id: repository_id.to_owned(),
        }
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
    /// When `Some`, only these repository IDs participate in the sync.
    /// Repositories present in storage but absent from this list are excluded
    /// from catalog scanning, edge resolution, and workspace snapshot
    /// provenance (§14 workspace membership is authoritative).
    pub active_repository_ids: Option<Vec<String>>,
}

impl Default for WorkspaceSyncOptions {
    fn default() -> Self {
        Self {
            deadline: Deadline::after(std::time::Duration::from_secs(30)),
            cancel: CancelToken::never(),
            active_repository_ids: None,
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
    /// Workspace snapshot ID that backs this sync result (provenance).
    pub snapshot_id: Option<String>,
}

/// Perform a full workspace sync: scan all repos, resolve, persist edges.
///
/// **Two-stage architecture:**
///
/// 1. **Reader phase** (read-only): For each repository, scan manifests from
///    disk via `scan_repository_manifests` (queries DB for indexed paths, reads
///    files through Phase 1B safe-content boundary). Build in-memory
///    `RepoCatalogData`. NO persistence occurs in this phase.
///
/// 2. **Writer phase** (single atomic closure): Persist all catalogs +
///    declarations, then resolve cross-repo edges and persist them.
///
/// The reader phase MUST NOT call `sync_repository` because that function
/// persists state. Manifest scanning is done via the read-only
/// `scan_repository_manifests` path.
pub fn sync_workspace(
    reader_conn: &rusqlite::Connection,
    writer: &attic_storage::WriterQueueHandle,
    opts: &WorkspaceSyncOptions,
) -> Result<WorkspaceSyncResult, CrossRepoError> {
    let mut result = WorkspaceSyncResult {
        repository_reports: Vec::new(),
        diagnostics: ResolutionDiagnostics::default(),
        edges_emitted: 0,
        snapshot_id: None,
    };

    // ── Stage 1: Reader phase (read-only, bounded I/O) ────────────────
    let all_ids = attic_storage::crossrepo_ops::all_repository_ids(reader_conn)?;
    // §14: workspace membership is authoritative. When active_repository_ids is
    // provided, restrict the sync to only those IDs. Repositories present in
    // storage but absent from the active set must not participate.
    let repo_ids: Vec<String> = match &opts.active_repository_ids {
        Some(active) => all_ids
            .into_iter()
            .filter(|id| active.contains(id))
            .collect(),
        None => all_ids,
    };
    let total = repo_ids.len();
    let mut all_repo_data: Vec<RepoCatalogData> = Vec::with_capacity(total);
    let mut proto_index: HashMap<String, Vec<String>> = HashMap::new();
    // Collect scans for later persistence (keyed by repo_id).
    let mut scans: HashMap<
        String,
        (
            crate::catalog::CatalogScan,
            Vec<crate::ProvidedIdentity>,
            Vec<crate::DependencyDeclaration>,
        ),
    > = HashMap::new();

    for (idx, repo_id) in repo_ids.iter().enumerate() {
        if opts.cancel.is_cancelled() || opts.deadline.expired() {
            debug!("workspace sync cancelled at repo {idx}/{total}");
            break;
        }

        // Scan manifests from disk (read-only — queries DB for paths, reads
        // files through Phase 1B safe-content boundary).
        let scan = match crate::catalog::scan_repository_manifests(reader_conn, repo_id) {
            Ok(s) => s,
            Err(e) => {
                debug!("failed to scan repo {}: {e}", repo_id);
                continue;
            }
        };

        // Extract provides and declarations from the scan.
        let mut provides = Vec::new();
        let mut declarations = Vec::new();
        for m in &scan.manifests {
            provides.extend(m.provides.clone());
            declarations.extend(m.declarations.clone());
        }

        // Resolve source revision from authoritative DB state.
        // Skip repositories without a valid SourceRevision (not yet indexed).
        let source_revision_id = match resolve_source_revision(reader_conn, repo_id) {
            Ok(id) => id,
            Err(CrossRepoError::NoSourceRevision { repository_id }) => {
                debug!(
                    "skipping repo {} — no source revision (not indexed)",
                    repository_id
                );
                result
                    .diagnostics
                    .missing_targets
                    .push((repository_id.clone(), "no_source_revision".to_owned()));
                continue;
            }
            Err(e) => return Err(e),
        };

        // Build resolver input from scan results (in-memory, no persistence).
        let repo_id_parsed = repo_id
            .parse::<attic_core::RepositoryId>()
            .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
        let root_path =
            attic_storage::get_repository_path(reader_conn, &repo_id_parsed)?.unwrap_or_default();

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
            provides: provides.clone(),
            declarations: declarations.clone(),
            primary_anchor_occurrence: primary,
            go_module_prefix: gmp,
        });

        // Store scan for later persistence in writer phase.
        let manifest_hash = scan.manifest_hash.clone();
        let oversized_count = scan.oversized.len();
        let unreadable_count = scan.unreadable.len();
        scans.insert(repo_id.clone(), (scan, provides, declarations));

        result.repository_reports.push(SyncReport {
            repository_id: repo_id.clone(),
            provides_count: all_repo_data.last().unwrap().provides.len(),
            declarations_count: all_repo_data.last().unwrap().declarations.len(),
            oversized: oversized_count,
            unreadable: unreadable_count,
            manifest_hash,
        });
    }

    // ── Pure computation: resolve cross-repo edges ────────────────────
    let (edges, diagnostics) = resolver::resolve_workspace(&all_repo_data, &proto_index);
    result.diagnostics = diagnostics;
    let edges_len = edges.len();

    // Build (repo_id, source_revision_id) pairs for snapshot provenance.
    let snapshot_revisions_input: Vec<(String, String)> = all_repo_data
        .iter()
        .map(|r| (r.repository_id.clone(), r.source_revision_id.clone()))
        .collect();

    // ── Stage 2: Writer phase (single atomic closure) ─────────────────
    let edges_clone = edges.clone();
    let repo_ids_for_persistence: Vec<String> = all_repo_data
        .iter()
        .map(|r| r.repository_id.clone())
        .collect();
    // Move scans into the closure for persistence.
    let scans_clone = scans;

    // Use a bounded channel (capacity 1) to return the snapshot_id from the
    // writer closure back to the caller.  This avoids any mutex and therefore
    // has no poisoning path that could cause a panic.
    //
    // Contract:
    //   - The writer closure sends exactly one value before returning Ok(()).
    //   - If the closure returns Err(...), writer.send() propagates the error
    //     and the try_recv() line below is never reached.
    //   - snap_rx cannot be dropped before the send because it lives on the
    //     same stack frame as the writer.send() call.
    let (snap_tx, snap_rx) = std::sync::mpsc::sync_channel::<String>(1);

    writer
        .send(move |conn| {
            // Persist catalog + declarations for each repository.
            for repo_id in &repo_ids_for_persistence {
                if let Some((scan, provides, declarations)) = scans_clone.get(repo_id) {
                    let source_revision_id = all_repo_data
                        .iter()
                        .find(|r| &r.repository_id == repo_id)
                        .map(|r| r.source_revision_id.clone())
                        .unwrap_or_default();
                    crate::catalog::persist_catalog(
                        conn,
                        repo_id,
                        &source_revision_id,
                        scan,
                        provides,
                        declarations,
                    )
                    .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
                }
            }

            // Create workspace snapshot (provenance record).
            let snapshot_id = attic_storage::crossrepo_ops::create_workspace_snapshot(
                conn,
                &snapshot_revisions_input,
                edges_clone.len(),
            )?;

            // Return the snapshot_id to the caller via the channel.
            // send() can only fail (Disconnected) if snap_rx was dropped, which
            // cannot happen because snap_rx lives on the enclosing stack frame.
            snap_tx.send(snapshot_id.clone()).map_err(|_| {
                attic_storage::StorageError::Worker(
                    "workspace sync aborted: snapshot_id channel disconnected".into(),
                )
            })?;

            // Delete all existing cross-repo DEPENDS_ON edges (clean replacement).
            for repo_id in &repo_ids_for_persistence {
                attic_storage::crossrepo_ops::delete_all_xrepo_edges_touching(conn, repo_id)?;
            }

            // Insert resolved edges, embedding snapshot_id in provenance_json.
            for e in &edges_clone {
                // Merge snapshot_id into existing provenance_json.
                let provenance = if e.provenance_json.trim().starts_with('{') {
                    let mut p: serde_json::Value =
                        serde_json::from_str(&e.provenance_json).unwrap_or(serde_json::json!({}));
                    p["workspace_snapshot_id"] = serde_json::Value::String(snapshot_id.clone());
                    p.to_string()
                } else {
                    serde_json::json!({ "workspace_snapshot_id": snapshot_id }).to_string()
                };

                attic_storage::crossrepo_ops::insert_xrepo_edge(
                    conn,
                    &e.source_repository_id,
                    &e.source_entity_id,
                    &e.target_repository_id,
                    &e.target_entity_id,
                    &e.resolution,
                    e.confidence,
                    &e.dependency_basis,
                    &provenance,
                    &e.source_revision_id,
                )?;
            }

            Ok(())
        })
        .map_err(CrossRepoError::Storage)?;

    // The writer closure returned Ok(()), which means it called snap_tx.send()
    // exactly once.  try_recv() is therefore always Ok here.  If for any reason
    // the value is absent (logic bug), we surface an error rather than panicking.
    let snapshot_id = snap_rx.try_recv().map_err(|_| {
        CrossRepoError::Storage(attic_storage::StorageError::Worker(
            "workspace sync internal error: snapshot_id not received after writer success".into(),
        ))
    })?;
    result.edges_emitted = edges_len;
    result.snapshot_id = Some(snapshot_id);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Convenience wrapper: membership-scoped workspace maintenance
// ---------------------------------------------------------------------------

/// Run a full workspace sync scoped to the given active repository IDs.
///
/// Equivalent to calling `sync_workspace` but restricts the sync to only the
/// repositories listed in `active_ids` (§14 membership is authoritative).
/// Repositories present in the database but absent from `active_ids` are
/// excluded from scanning, edge resolution and snapshot provenance.
///
/// `pool` provides the reader connection; `writer` is the single write queue.
pub fn run_workspace_maintenance_with_membership(
    pool: &attic_storage::DbPool,
    writer: &attic_storage::WriterQueueHandle,
    active_ids: Vec<String>,
) -> Result<WorkspaceSyncResult, CrossRepoError> {
    let opts = WorkspaceSyncOptions {
        active_repository_ids: Some(active_ids),
        ..Default::default()
    };
    pool.with_reader(|conn| {
        sync_workspace(conn, writer, &opts)
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    })
    .map_err(CrossRepoError::Storage)
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
///
/// On manifest change:
/// 1. Invalidate outgoing edges (this repo's declarations changed).
/// 2. If this repo provides packages, invalidate incoming edges (provides changed).
/// 3. Re-sync catalog and re-resolve cross-repo edges for affected repos.
pub fn incremental_sync(
    conn: &rusqlite::Connection,
    repository_id: &str,
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
        "manifest change detected, incremental recomputation"
    );

    // 1. Invalidate OUTGOING edges from this repo (declarations changed).
    let out_stale = conn.execute(
        "UPDATE core_relationships SET freshness_state = 'STALE'
          WHERE source_repository_id = ?1
            AND rel_type = 'DEPENDS_ON'
            AND freshness_state = 'CURRENT'",
        rusqlite::params![repository_id],
    )?;
    debug!(
        repo = repository_id,
        out_stale = out_stale,
        "invalidated outgoing edges"
    );

    // 2. Check if this repo provides any packages. If so, invalidate INCOMING edges
    // (its provides changed, consumers' resolution may need update).
    let provides_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM core_dependency_declarations
           WHERE repository_id = ?1 AND json_type(provides_json) = 'array'",
            rusqlite::params![repository_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let in_stale = if provides_count > 0 {
        conn.execute(
            "UPDATE core_relationships SET freshness_state = 'STALE'
              WHERE target_repository_id = ?1
                AND rel_type = 'DEPENDS_ON'
                AND freshness_state = 'CURRENT'",
            rusqlite::params![repository_id],
        )?
    } else {
        0
    };
    debug!(
        repo = repository_id,
        in_stale = in_stale,
        "invalidated incoming edges"
    );

    // 3. Re-sync this repository's catalog with fresh SourceRevision.
    sync_repository(conn, repository_id)?;

    // 4. Re-resolve cross-repo edges for ALL repos that had edges invalidated.
    // Build resolver input for the whole workspace (bounded) and persist new edges.
    // This is a targeted recomputation, not a full workspace rebuild.
    let repo_ids = attic_storage::crossrepo_ops::all_repository_ids(conn)?;
    let mut all_repo_data: Vec<crate::resolver::RepoCatalogData> =
        Vec::with_capacity(repo_ids.len());
    let mut proto_index: HashMap<String, Vec<String>> = HashMap::new();

    for rid in &repo_ids {
        // Scan manifests from disk (read-only).
        let scan = match crate::catalog::scan_repository_manifests(conn, rid) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut provides = Vec::new();
        let mut declarations = Vec::new();
        for m in &scan.manifests {
            provides.extend(m.provides.clone());
            declarations.extend(m.declarations.clone());
        }
        let source_revision_id = resolve_source_revision(conn, rid)?;
        let repo_id_parsed = rid
            .parse::<attic_core::RepositoryId>()
            .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
        let root_path =
            attic_storage::get_repository_path(conn, &repo_id_parsed)?.unwrap_or_default();
        let gmp = provides
            .iter()
            .find(|p| p.ecosystem == crate::Ecosystem::Go)
            .map(|p| p.name.clone());
        let primary = crate::catalog::primary_anchor_for_repo(conn, rid);
        let proto_specs = crate::catalog::scan_proto_imports(conn, rid)?;
        if !proto_specs.is_empty() {
            proto_index.insert(rid.clone(), proto_specs);
        }
        all_repo_data.push(crate::resolver::RepoCatalogData {
            repository_id: rid.clone(),
            root_path,
            source_revision_id,
            provides: provides.clone(),
            declarations: declarations.clone(),
            primary_anchor_occurrence: primary,
            go_module_prefix: gmp,
        });
    }

    // Resolve and persist new edges (replaces stale ones).
    let (edges, _diag) = crate::resolver::resolve_workspace(&all_repo_data, &proto_index);

    // Delete ALL stale edges originating from this repository before inserting new ones.
    // Without this, removing a dependency leaves the old stale edge orphaned.
    conn.execute(
        "DELETE FROM core_relationships
         WHERE source_repository_id = ?1
           AND rel_type = 'DEPENDS_ON'
           AND freshness_state = 'STALE'",
        rusqlite::params![repository_id],
    )?;

    for e in &edges {
        // Insert fresh edge.
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
        let resynced = incremental_sync(&conn, &tid("r1"), &changed).unwrap();
        assert!(!resynced, "non-manifest changes should not trigger resync");
    }

    #[test]
    fn incremental_sync_triggers_on_manifest_change() {
        let conn = seeded_conn();
        insert_repo(&conn, "r1", "/ws/r1");
        let _rev = insert_rev(&conn, "r1");

        let changed = vec!["src/main.rs".to_owned(), "go.mod".to_owned()];
        let resynced = incremental_sync(&conn, &tid("r1"), &changed).unwrap();
        assert!(resynced, "manifest change should trigger resync");
    }

    #[test]
    fn invalidate_edges_targeting_marks_stale() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");

        let edge_id = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r0"),
            "s0",
            &tid("r1"),
            "t1",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev,
        )
        .unwrap();

        // Also insert an edge NOT targeting r1
        let _edge_id2 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r0"),
            "s0",
            &tid("r0"),
            "t0",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev,
        )
        .unwrap();

        let count = invalidate_edges_targeting(&conn, &tid("r1")).unwrap();
        assert_eq!(
            count, 1,
            "only the edge targeting r1 should be marked STALE"
        );

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
            &conn,
            &tid("r0"),
            "s0",
            &tid("r1"),
            "t1",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev,
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
        let _rev = insert_rev(&conn, "test-repo");

        let report = sync_repository(&conn, &tid("test-repo")).unwrap();
        assert_eq!(report.repository_id, tid("test-repo"));
        assert!(
            !report.manifest_hash.is_empty(),
            "manifest_hash should be computed"
        );

        // Verify catalog row exists
        let catalog =
            attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("test-repo")).unwrap();
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
        let _rev = insert_rev(&conn, "r1");

        let report = sync_repository(&conn, &tid("r1")).unwrap();
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

        sync_repository(&conn, &tid("src-rev-test")).unwrap();

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
        attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &cat0, &cat0.provides_json)
            .unwrap();

        let cat1 = attic_storage::crossrepo_ops::CatalogRow {
            repository_id: tid("r1"),
            source_revision_id: rev1.clone(),
            provides_json: "[]".to_owned(),
            manifest_hash: "hash1".to_owned(),
            entry_count: 0,
            freshness_state: "CURRENT".to_owned(),
        };
        attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &cat1, &cat1.provides_json)
            .unwrap();

        // Insert edges r0→r1 and r1→r0
        let _e1 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r0"),
            "s0",
            &tid("r1"),
            "t1",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev0,
        )
        .unwrap();
        let _e2 = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r1"),
            "s1",
            &tid("r0"),
            "t0",
            "PACKAGE_RESOLVED",
            0.8,
            "GO_MODULE",
            "{}",
            &rev1,
        )
        .unwrap();

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
        let result = incremental_sync(&conn, &tid("nonexistent"), &["go.mod".to_owned()]);
        assert!(result.is_err(), "should propagate error for missing repo");
    }

    /// Prove that `sync_workspace` snapshot provenance delivery cannot panic.
    ///
    /// The previous implementation used `Arc<Mutex<Option<String>>>` with
    /// `.lock().unwrap()` — if the mutex was poisoned the writer thread and
    /// the caller would both panic.  The replacement uses a `sync_channel`,
    /// which has no poisoning path.
    ///
    /// This test exercises the channel round-trip end-to-end:
    ///  - `sync_workspace` with an empty active set succeeds,
    ///  - `snapshot_id` is populated (the channel sent/received correctly),
    ///  - no panic occurs.
    ///
    /// Additionally, a forced writer-closure failure (injected via an
    /// empty/poisoned active set that triggers an Err return from the
    /// closure — achieved via a storage error from the writer handle) must
    /// propagate as `Err(CrossRepoError::Storage(...))`, not as a panic.
    #[test]
    fn sync_workspace_snapshot_provenance_channel_never_panics() {
        use attic_storage::writer::WriterQueue;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("provenance_test.db");

        // Seed the DB then close the seeding connection.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            configure_connection(&conn).unwrap();
            run_migrations(&conn).unwrap();
        }

        let writer_conn = rusqlite::Connection::open(&db_path).unwrap();
        configure_connection(&writer_conn).unwrap();
        let wq = WriterQueue::new(writer_conn).unwrap();
        let writer_handle = wq.handle();

        let reader_conn = rusqlite::Connection::open(&db_path).unwrap();
        configure_connection(&reader_conn).unwrap();

        // Happy path: empty active set → no repos scanned → snapshot still
        // created and snapshot_id returned via channel (no panic).
        let opts = WorkspaceSyncOptions {
            active_repository_ids: Some(vec![]),
            ..Default::default()
        };
        let result = sync_workspace(&reader_conn, &writer_handle, &opts);
        assert!(
            result.is_ok(),
            "sync_workspace with empty active set must succeed, got {result:?}"
        );
        let ws_result = result.unwrap();
        assert!(
            ws_result.snapshot_id.is_some(),
            "snapshot_id must be populated after successful sync (channel round-trip ok)"
        );
        assert_eq!(ws_result.repository_reports.len(), 0);
        assert_eq!(ws_result.edges_emitted, 0);

        // Failure path: drop the writer queue (ShutDown) then call sync_workspace —
        // must return Err, not panic.
        drop(wq);
        let opts2 = WorkspaceSyncOptions {
            active_repository_ids: Some(vec![]),
            ..Default::default()
        };
        let result2 = sync_workspace(&reader_conn, &writer_handle, &opts2);
        assert!(
            result2.is_err(),
            "sync_workspace must return Err when writer is shut down, not panic"
        );
    }

    /// §14 / §16 — `active_repository_ids` must exclude stale DB repos from
    /// catalog scanning, edge resolution and snapshot provenance.
    ///
    /// Three repositories (r_a, r_b, r_c) are seeded into the DB.
    /// `sync_workspace` is called with `active_repository_ids = Some([r_a, r_c])`.
    /// r_b must not appear in `repository_reports` and must not participate in
    /// any cross-repo edges or the workspace snapshot.
    #[test]
    fn sync_workspace_active_ids_excludes_stale() {
        use attic_storage::writer::WriterQueue;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        // Phase 1: seed three repos via a dedicated connection (closed before
        // the writer/reader connections are opened to avoid WAL contention).
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            configure_connection(&conn).unwrap();
            run_migrations(&conn).unwrap();
            insert_repo(&conn, "r_a", "/tmp/r_a");
            insert_repo(&conn, "r_b", "/tmp/r_b");
            insert_repo(&conn, "r_c", "/tmp/r_c");
            insert_rev(&conn, "r_a");
            insert_rev(&conn, "r_b");
            insert_rev(&conn, "r_c");
        }

        // Phase 2: open separate writer and reader connections.
        let writer_conn = rusqlite::Connection::open(&db_path).unwrap();
        configure_connection(&writer_conn).unwrap();
        let wq = WriterQueue::new(writer_conn).unwrap();
        let writer_handle = wq.handle();

        let reader_conn = rusqlite::Connection::open(&db_path).unwrap();
        configure_connection(&reader_conn).unwrap();

        let a_id = tid("r_a");
        let b_id = tid("r_b");
        let c_id = tid("r_c");

        // Sync only r_a and r_c — r_b is present in the DB but excluded.
        let opts = WorkspaceSyncOptions {
            active_repository_ids: Some(vec![a_id.clone(), c_id.clone()]),
            ..Default::default()
        };
        let result = sync_workspace(&reader_conn, &writer_handle, &opts)
            .expect("sync_workspace must succeed");

        // r_b must NOT appear in repository_reports.
        let reported_ids: Vec<&str> = result
            .repository_reports
            .iter()
            .map(|r| r.repository_id.as_str())
            .collect();
        assert!(
            !reported_ids.contains(&b_id.as_str()),
            "r_b must be excluded from sync when not in active_repository_ids; \
             reported: {reported_ids:?}"
        );

        // Verify no cross-repo edges involving r_b were written.
        let b_edge_count: i64 = reader_conn
            .query_row(
                "SELECT COUNT(*) FROM core_relationships \
                  WHERE source_repository_id = ?1 OR target_repository_id = ?1",
                rusqlite::params![b_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        assert_eq!(
            b_edge_count, 0,
            "no cross-repo edges must reference the excluded repo r_b"
        );
    }
}
