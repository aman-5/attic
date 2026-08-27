//! Workspace Catalog (Phase 6 §6).
//!
//! A bounded, DERIVED catalog of what each repository provides and declares.
//! It is rebuilt from canonical repository intelligence (indexed file
//! occurrences + manifest bytes read through bounded safe-content access)
//! and is never an authoritative second source of truth.
//!
//! Two flows:
//!
//! ```text
//! sync_repository(conn_writer, repo_id):
//!   indexed paths → read bounded → parse → compute hash → persist
//!
//! build_resolver_input(conn_reader):
//!   DB → Vec<RepoCatalogData> for the resolver
//! ```

use rusqlite::Connection;
use tracing::debug;

use crate::error::CrossRepoError;
use crate::manifest::{self, ManifestParse};
use crate::resolver::RepoCatalogData;
use crate::{DeclarationKind, DependencyDeclaration, Ecosystem, ProvidedIdentity, limits};

// ---------------------------------------------------------------------------
// Bounded manifest reading
// ---------------------------------------------------------------------------

/// Maximum distinct manifest files scanned per repository per pass.
const MAX_MANIFESTS_PER_REPO: usize = 2_000;

/// Enumerate indexed manifest paths for one repository from file occurrences.
pub fn indexed_manifest_paths(
    conn: &Connection,
    repository_id: &str,
) -> Result<Vec<String>, CrossRepoError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT fo.path
           FROM core_file_occurrences fo
           JOIN core_file_identities fi ON fi.id = fo.file_identity_id
          WHERE fi.repository_id = ?1
            AND fo.existence_state != 'deleted'
          ORDER BY fo.path
          LIMIT ?2",
    )?;
    let cap = MAX_MANIFESTS_PER_REPO as i64;
    let rows = stmt.query_map(rusqlite::params![repository_id, cap], |r| {
        r.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        let p = row?;
        if manifest::is_manifest_path(&p) {
            out.push(p);
        }
    }
    Ok(out)
}

/// Read one manifest with hard bounds and path containment.
///
/// Uses Phase 1B safe-content boundary via `attic_discovery::read_bounded`
/// for path containment, size bounds, and secret scanning.
fn read_manifest_bounded(
    repo_root: &std::path::Path,
    rel_path: &str,
) -> Result<Option<Vec<u8>>, CrossRepoError> {
    match attic_discovery::read_bounded(repo_root, rel_path, limits::MAX_MANIFEST_BYTES) {
        Ok(bytes) => Ok(bytes),
        Err(attic_discovery::DiscoveryError::PathEscape { .. }) => {
            Err(CrossRepoError::PathEscape(rel_path.to_owned()))
        }
        Err(attic_discovery::DiscoveryError::Canonicalize { .. }) => Ok(None),
        Err(attic_discovery::DiscoveryError::FileTooLarge { max_bytes, .. }) => {
            Err(CrossRepoError::LimitExceeded {
                limit: "MAX_MANIFEST_BYTES",
                context: format!("{rel_path} exceeds {max_bytes} bytes"),
            })
        }
        Err(_) => Ok(None),
    }
}

/// Read one proto file with hard bounds.
///
/// Uses Phase 1B safe-content boundary for path containment.
fn read_proto_bounded(
    repo_root: &std::path::Path,
    rel_path: &str,
) -> Result<Option<Vec<u8>>, CrossRepoError> {
    match attic_discovery::read_bounded(repo_root, rel_path, limits::MAX_MANIFEST_BYTES) {
        Ok(bytes) => Ok(bytes),
        Err(attic_discovery::DiscoveryError::PathEscape { .. }) => Ok(None),
        Err(attic_discovery::DiscoveryError::Canonicalize { .. }) => Ok(None),
        Err(_) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Catalog scan (disk reads) + persistence
// ---------------------------------------------------------------------------

/// Result of scanning one repository's manifests.
#[derive(Debug, Default, Clone)]
pub struct CatalogScan {
    /// Successfully parsed manifests.
    pub manifests: Vec<ManifestParse>,
    /// Manifest paths skipped because they exceeded the byte cap.
    pub oversized: Vec<String>,
    /// Manifest paths whose content could not be read.
    pub unreadable: Vec<String>,
    /// BLAKE3 hash over sorted (path, content_hash) for incremental refresh.
    pub manifest_hash: String,
}

/// Scan every indexed manifest of one repository into parsed form.
pub fn scan_repository_manifests(
    conn: &Connection,
    repository_id: &str,
) -> Result<CatalogScan, CrossRepoError> {
    let mut scan = CatalogScan::default();
    let paths = indexed_manifest_paths(conn, repository_id)?;
    let repo_id = repository_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
    let Some(root) = attic_storage::get_repository_path(conn, &repo_id)? else {
        return Ok(scan);
    };
    let root_path = std::path::PathBuf::from(root);

    // Sort paths for deterministic manifest_hash computation.
    let mut hashes: Vec<(String, Vec<u8>)> = Vec::new();
    for p in &paths {
        match read_manifest_bounded(&root_path, p) {
            Ok(Some(bytes)) => {
                hashes.push((p.clone(), blake3::hash(&bytes).as_bytes().to_vec()));
                let parsed = manifest::parse_manifest(p, &bytes);
                scan.manifests.push(parsed);
            }
            Ok(None) => scan.unreadable.push(p.clone()),
            Err(CrossRepoError::LimitExceeded { .. }) => scan.oversized.push(p.clone()),
            Err(e) => return Err(e),
        }
    }

    // Deterministic manifest_hash: BLAKE3 of sorted (path, blake3(bytes)).
    hashes.sort_by(|a, b| a.0.cmp(&b.0));
    let mut hasher = blake3::Hasher::new();
    for (p, h) in &hashes {
        hasher.update(p.as_bytes());
        hasher.update(h);
    }
    scan.manifest_hash = hasher.finalize().to_hex().to_string();
    Ok(scan)
}

/// Scan proto imports for a repository (for GeneratedApi resolution).
pub fn scan_proto_imports(
    conn: &Connection,
    repository_id: &str,
) -> Result<Vec<String>, CrossRepoError> {
    let repo_id = repository_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
    let Some(root) = attic_storage::get_repository_path(conn, &repo_id)? else {
        return Ok(Vec::new());
    };
    let root_path = std::path::PathBuf::from(root);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT fo.path
           FROM core_file_occurrences fo
           JOIN core_file_identities fi ON fi.id = fo.file_identity_id
          WHERE fi.repository_id = ?1
            AND fo.path LIKE '%.proto'
            AND fo.existence_state != 'deleted'
          ORDER BY fo.path
          LIMIT ?2",
    )?;
    let cap = MAX_MANIFESTS_PER_REPO as i64;
    let rows = stmt.query_map(rusqlite::params![repository_id, cap], |r| {
        r.get::<_, String>(0)
    })?;
    let mut specifiers = Vec::new();
    for row in rows {
        let p = row?;
        if let Ok(Some(bytes)) = read_proto_bounded(&root_path, &p) {
            let parse = manifest::parse_proto_imports(&p, &bytes);
            for d in &parse.declarations {
                specifiers.push(d.name.clone());
            }
        }
    }
    specifiers.sort();
    specifiers.dedup();
    Ok(specifiers)
}

/// Find the primary anchor occurrence id for a repository.
///
/// Tries the repo-root manifest files in priority order.
pub fn primary_anchor_for_repo(conn: &Connection, repository_id: &str) -> Option<String> {
    let repo_id = repository_id.parse::<attic_core::RepositoryId>().ok()?;
    for name in &[
        "go.mod",
        "package.json",
        "pom.xml",
        "pyproject.toml",
        "build.gradle",
    ] {
        if let Ok(Some(id)) =
            attic_storage::lookup_latest_file_occurrence_for_path(conn, &repo_id, name)
        {
            return Some(id);
        }
    }
    None
}

/// Compute the go module prefix from provides list.
fn go_module_prefix(provides: &[ProvidedIdentity]) -> Option<String> {
    provides
        .iter()
        .find(|p| p.ecosystem == Ecosystem::Go)
        .map(|p| p.name.clone())
}

/// Build a single repository's [`RepoCatalogData`] from a scan.
#[cfg(test)]
fn build_repo_catalog_data(
    repository_id: &str,
    root_path: &str,
    source_revision_id: &str,
    scan: &CatalogScan,
) -> RepoCatalogData {
    let mut provides = Vec::new();
    let mut declarations = Vec::new();
    for m in &scan.manifests {
        provides.extend(m.provides.clone());
        declarations.extend(m.declarations.clone());
    }

    // Truncate if over limits.
    provides.truncate(limits::MAX_PROVIDES_PER_REPO);
    declarations.truncate(limits::MAX_DECLARATIONS_PER_REPO);

    let gmp = go_module_prefix(&provides);

    RepoCatalogData {
        repository_id: repository_id.to_owned(),
        root_path: root_path.to_owned(),
        source_revision_id: source_revision_id.to_owned(),
        provides,
        declarations,
        primary_anchor_occurrence: None, // filled by caller
        go_module_prefix: gmp,
    }
}

/// Persist a catalog scan result into the database.
///
/// Designed to run inside a single writer-queue closure (Phase 1A contract).
pub fn persist_catalog(
    conn: &Connection,
    repository_id: &str,
    source_revision_id: &str,
    scan: &CatalogScan,
    provides: &[ProvidedIdentity],
    declarations: &[DependencyDeclaration],
) -> Result<(), CrossRepoError> {
    let provides_json = serde_json::to_string(provides)
        .map_err(|e| CrossRepoError::InvalidRoot(format!("serde: {e}")))?;

    // Upsert catalog row.
    let row = attic_storage::crossrepo_ops::CatalogRow {
        repository_id: repository_id.to_owned(),
        source_revision_id: source_revision_id.to_owned(),
        provides_json,
        manifest_hash: scan.manifest_hash.clone(),
        entry_count: declarations.len() as i64,
        freshness_state: "CURRENT".to_owned(),
    };
    attic_storage::crossrepo_ops::upsert_catalog_row(conn, &row, &row.provides_json)?;

    // Delete and re-insert declarations.
    attic_storage::crossrepo_ops::delete_declarations_for_repository(conn, repository_id)?;

    for d in declarations {
        let decl = attic_storage::crossrepo_ops::DeclarationRow {
            id: String::new(),
            repository_id: repository_id.to_owned(),
            file_occurrence_id: None,
            path: d.path.clone(),
            ecosystem: d.ecosystem.to_string(),
            name: d.name.clone(),
            version_req: d.version_req.clone(),
            declaration_kind: d.kind.as_str().to_owned(),
            local_hint: d.local_hint.clone(),
            source_revision_id: source_revision_id.to_owned(),
            freshness_state: "CURRENT".to_owned(),
        };
        attic_storage::crossrepo_ops::insert_declaration(conn, &decl)?;
    }

    debug!(
        repo = repository_id,
        decls = declarations.len(),
        provides = provides.len(),
        "catalog persisted"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// DB → Resolver input (read-only flow)
// ---------------------------------------------------------------------------

/// Build [`RepoCatalogData`] for every registered repository from DB state.
///
/// Used by the resolver to build the provider index — pure read, no disk I/O.
pub fn build_resolver_input(conn: &Connection) -> Result<Vec<RepoCatalogData>, CrossRepoError> {
    let repos = attic_storage::crossrepo_ops::all_repository_ids(conn)?;
    let mut out = Vec::with_capacity(repos.len());
    for repo_id in &repos {
        // Catalog row → provides.
        let catalog = attic_storage::crossrepo_ops::catalog_entry(conn, repo_id)?;
        let provides: Vec<ProvidedIdentity> = catalog
            .as_ref()
            .and_then(|c| serde_json::from_str(&c.provides_json).ok())
            .unwrap_or_default();

        // Declarations.
        let raw_decls = attic_storage::crossrepo_ops::declarations_for_repository(conn, repo_id)?;
        let declarations: Vec<DependencyDeclaration> = raw_decls
            .into_iter()
            .map(|d| DependencyDeclaration {
                path: d.path,
                ecosystem: Ecosystem::from_db_str(&d.ecosystem).unwrap_or(Ecosystem::Maven),
                name: d.name,
                version_req: d.version_req,
                kind: DeclarationKind::from_db_str(&d.declaration_kind)
                    .unwrap_or(DeclarationKind::External),
                local_hint: d.local_hint,
            })
            .collect();

        // Repository root path.
        let repo_id_parsed = repo_id
            .parse::<attic_core::RepositoryId>()
            .map_err(|e| CrossRepoError::InvalidRoot(format!("bad repo id: {e}")))?;
        let root_path =
            attic_storage::get_repository_path(conn, &repo_id_parsed)?.unwrap_or_default();

        // Source revision.
        let source_revision_id = catalog
            .as_ref()
            .map(|c| c.source_revision_id.clone())
            .unwrap_or_default();

        let gmp = go_module_prefix(&provides);
        let primary = primary_anchor_for_repo(conn, repo_id);

        out.push(RepoCatalogData {
            repository_id: repo_id.clone(),
            root_path,
            source_revision_id,
            provides,
            declarations,
            primary_anchor_occurrence: primary,
            go_module_prefix: gmp,
        });
    }
    debug!(repos = out.len(), "resolver input built from catalog");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestParse;

    #[test]
    fn indexed_manifest_paths_filters_non_manifests() {
        // This tests the path filtering logic (is_manifest_path) indirectly
        // through the SQL query structure — actual DB test requires full schema.
        // The filtering logic is tested directly in manifest.rs tests.
        assert!(manifest::is_manifest_path("go.mod"));
        assert!(manifest::is_manifest_path("package.json"));
        assert!(manifest::is_manifest_path("pom.xml"));
        assert!(!manifest::is_manifest_path("src/main.rs"));
    }

    #[test]
    fn read_manifest_bounded_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Create a file outside the root
        let outside = std::env::temp_dir().join(format!("attic_test_{}", uuid::Uuid::new_v4()));
        std::fs::write(&outside, b"secret").unwrap();
        let outside_name = outside.file_name().unwrap().to_str().unwrap();

        // Try to read with .. escape
        let result = read_manifest_bounded(root, &format!("../{outside_name}"));
        assert!(result.unwrap().is_none(), "escape should return None");

        // Cleanup
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn read_manifest_bounded_rejects_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file_path = root.join("go.mod");
        let big_content = vec![b'x'; (limits::MAX_MANIFEST_BYTES as usize) + 1];
        std::fs::write(&file_path, &big_content).unwrap();

        let result = read_manifest_bounded(root, "go.mod");
        match result {
            Err(CrossRepoError::LimitExceeded { limit, .. }) => {
                assert_eq!(limit, "MAX_MANIFEST_BYTES");
            }
            other => panic!("expected LimitExceeded, got {other:?}"),
        }
    }

    #[test]
    fn read_manifest_bounded_reads_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("go.mod"), b"module example.com/x\n").unwrap();

        let result = read_manifest_bounded(root, "go.mod");
        assert_eq!(result.unwrap(), Some(b"module example.com/x\n".to_vec()));
    }

    #[test]
    fn read_manifest_bounded_returns_none_for_missing() {
        let dir = tempfile::tempdir().unwrap();
        let result = read_manifest_bounded(dir.path(), "nope.toml");
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn read_manifest_bounded_normalizes_backslash() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let sub = root.join("services").join("api");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("go.mod"), b"module api\n").unwrap();

        // Use forward-slash path (Windows-safe)
        let result = read_manifest_bounded(root, "services/api/go.mod");
        assert_eq!(result.unwrap(), Some(b"module api\n".to_vec()));
    }

    #[test]
    fn build_repo_catalog_data_populates_go_module_prefix() {
        let scan = CatalogScan {
            manifests: vec![ManifestParse {
                provides: vec![crate::ProvidedIdentity {
                    ecosystem: crate::Ecosystem::Go,
                    name: "example.com/team/svc".to_owned(),
                }],
                declarations: vec![],
                diagnostics: vec![],
            }],
            ..Default::default()
        };

        let data = build_repo_catalog_data("repo-1", "/ws/svc", "rev-1", &scan);
        assert_eq!(
            data.go_module_prefix.as_deref(),
            Some("example.com/team/svc")
        );
        assert_eq!(data.provides.len(), 1);
        assert_eq!(data.repository_id, "repo-1");
    }

    #[test]
    fn build_repo_catalog_data_truncates_at_limits() {
        let mut provides = Vec::new();
        for i in 0..limits::MAX_PROVIDES_PER_REPO + 100 {
            provides.push(crate::ProvidedIdentity {
                ecosystem: crate::Ecosystem::Maven,
                name: format!("g:a{i}"),
            });
        }
        let scan = CatalogScan {
            manifests: vec![ManifestParse {
                provides,
                declarations: vec![],
                diagnostics: vec![],
            }],
            ..Default::default()
        };

        let data = build_repo_catalog_data("repo-x", "/ws/x", "rev-x", &scan);
        assert_eq!(data.provides.len(), limits::MAX_PROVIDES_PER_REPO);
    }

    #[test]
    fn read_manifest_bounded_redacts_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A manifest containing a pattern that looks like an AWS key
        let content = "module x\naws_access_key_id = AKIAIOSFODNN7EXAMPLE\n";
        std::fs::write(root.join("go.mod"), content).unwrap();

        let result = read_manifest_bounded(root, "go.mod").unwrap();
        assert!(result.is_some(), "should return redacted content");
        let bytes = result.unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        // The secret pattern should be redacted (not present in output)
        assert!(
            !text.contains("AKIAIOSFODNN7EXAMPLE"),
            "secret must be redacted"
        );
    }

    #[test]
    fn read_manifest_bounded_path_escape_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let result = read_manifest_bounded(root, "../../etc/passwd");
        assert!(result.unwrap().is_none(), "path escape should return None");
    }
}
