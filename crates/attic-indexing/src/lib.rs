//! `attic-indexing` — End-to-end indexing pipeline (Phase 1D).
//!
//! Wires together:
//! - Phase 1B: `attic-discovery` (file walk + secrets classification)
//! - Phase 1C: `attic-analyzers` (retrieval-unit extraction)
//! - Phase 1A: `attic-storage` (FTS-backed SQLite persistence)
//!
//! # Entry point
//!
//! ```text
//! index_repository(conn, root, policy, opts) -> IndexResult
//! ```
//!
//! The caller is responsible for:
//! - Opening and migrating the database before calling `index_repository`.
//! - Providing a canonical absolute path for `root`.
//!
//! # Stdout / stderr contract
//!
//! This crate MUST NOT write to stdout.  Diagnostics go to `tracing` (stderr
//! in the server binary).

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::path::Path;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;
use tracing::{debug, info, warn};

use std::sync::Arc;

use attic_analyzers::{AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer, ResourceBudget};
use attic_core::{
    FileOccurrenceId, IndexGenerationId, RepositoryId, RetrievalUnitId, SourceRevisionId,
    FileType,
};
use attic_discovery::{
    DiscoveryPolicy, DownstreamClassification,
    preprocess_file_content,
};
use attic_storage::{NewRetrievalUnit, StorageError, delete_retrieval_units_for_file,
    insert_retrieval_unit_with_fts, run_migrations};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by the indexing pipeline.
#[derive(Debug, Error)]
pub enum IndexError {
    /// Discovery phase failure.
    #[error("discovery failed: {0}")]
    Discovery(#[from] attic_discovery::DiscoveryError),

    /// Storage operation failure.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// SQLite error not wrapped by StorageError.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// I/O error during file preprocessing.
    #[error("I/O error preprocessing {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Options controlling indexing behaviour.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Human-readable display name for the repository (used in DB record).
    pub repository_name: String,
    /// Maximum retrieval units to extract per file.
    /// Passed to the analyzer as a resource budget hint.
    pub max_units_per_file: usize,
    /// Whether to delete existing retrieval units for a file before
    /// re-indexing it.  Set `true` for incremental re-index passes.
    pub refresh_existing: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            repository_name: "default".to_owned(),
            max_units_per_file: 512,
            refresh_existing: true,
        }
    }
}

/// Summary statistics returned after a completed indexing run.
#[derive(Debug, Default, Clone)]
pub struct IndexResult {
    /// Number of files visited.
    pub files_visited: usize,
    /// Number of files successfully indexed.
    pub files_indexed: usize,
    /// Number of files skipped (excluded, very-large, I/O error, …).
    pub files_skipped: usize,
    /// Total retrieval units inserted.
    pub units_inserted: usize,
    /// Total retrieval units deleted (refresh_existing only).
    pub units_deleted: usize,
    /// Repository UUID used for this run.
    pub repository_id: String,
    /// Index generation UUID for this run.
    pub index_generation_id: String,
}

// ---------------------------------------------------------------------------
// Primary entry point
// ---------------------------------------------------------------------------

/// Index a repository root directory into `conn`.
///
/// Runs discovery, analyzes every eligible file, and upserts the resulting
/// retrieval units into the FTS-backed SQLite store.
///
/// `conn` **must** already have migrations applied (call
/// `attic_storage::run_migrations` before this function).
pub fn index_repository(
    conn: &Connection,
    root: &Path,
    policy: &DiscoveryPolicy,
    opts: &IndexOptions,
) -> Result<IndexResult, IndexError> {
    // -----------------------------------------------------------------------
    // 1. Ensure migration is applied.
    // -----------------------------------------------------------------------
    run_migrations(conn)?;

    // -----------------------------------------------------------------------
    // 2. Bootstrap or retrieve the repository record.
    // -----------------------------------------------------------------------
    let repo_id = ensure_repository(conn, root, &opts.repository_name)?;

    // -----------------------------------------------------------------------
    // 3. Create a new index generation record.
    // -----------------------------------------------------------------------
    let (gen_id, rev_id) = create_index_generation(conn, &repo_id)?;

    info!(
        repository_id = %repo_id,
        index_generation_id = %gen_id,
        root = %root.display(),
        "starting indexing run"
    );

    // -----------------------------------------------------------------------
    // 4. Run discovery.
    // -----------------------------------------------------------------------
    let discovery = attic_discovery::discover(root, policy)?;

    info!(
        files = discovery.entries.len(),
        "discovery complete"
    );

    // -----------------------------------------------------------------------
    // 5. Build analyzer registry.
    // -----------------------------------------------------------------------
    let registry = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()));

    // -----------------------------------------------------------------------
    // 6. Index each eligible file.
    // -----------------------------------------------------------------------
    let mut result = IndexResult {
        repository_id: repo_id.clone(),
        index_generation_id: gen_id.clone(),
        ..Default::default()
    };

    for entry in &discovery.entries {
        result.files_visited += 1;

        // Find the classification for this file.
        let classification = discovery
            .downstream_classifications
            .iter()
            .find(|(path, _)| path == &entry.repo_relative)
            .map(|(_, c)| c);

        // Skip excluded files.
        if matches!(classification, Some(DownstreamClassification::Excluded { .. })) {
            debug!(path = %entry.repo_relative, "skipping excluded file");
            result.files_skipped += 1;
            continue;
        }

        match index_single_file(
            conn,
            &entry.abs_path,
            &entry.repo_relative,
            &repo_id,
            &rev_id,
            &gen_id,
            &registry,
            opts,
        ) {
            Ok((inserted, deleted)) => {
                result.files_indexed += 1;
                result.units_inserted += inserted;
                result.units_deleted += deleted;
            }
            Err(e) => {
                warn!(
                    path = %entry.repo_relative,
                    error = %e,
                    "skipping file due to indexing error"
                );
                result.files_skipped += 1;
            }
        }
    }

    info!(
        files_indexed = result.files_indexed,
        files_skipped = result.files_skipped,
        units_inserted = result.units_inserted,
        "indexing run complete"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Per-file indexing
// ---------------------------------------------------------------------------

fn index_single_file(
    conn: &Connection,
    abs_path: &Path,
    repo_relative: &str,
    repository_id: &str,
    source_revision_id: &str,
    index_generation_id: &str,
    registry: &AnalyzerRegistry,
    opts: &IndexOptions,
) -> Result<(usize, usize), IndexError> {
    // 6a. Preprocess content (secrets scan + optional redaction).
    let preprocessed = preprocess_file_content(abs_path, repo_relative).map_err(|source| {
        IndexError::Io {
            path: repo_relative.to_owned(),
            source,
        }
    })?;

    // 6b. Determine whether this content is safe / redacted.
    let is_redacted = matches!(
        preprocessed.decision,
        attic_discovery::SecretScanDecision::Redacted
    );

    let content = match preprocessed.content {
        Some(c) => c,
        None => {
            // LARGE file with a streaming handle — skip for now (Phase 2).
            debug!(path = %repo_relative, "skipping LARGE streaming file in Phase 1D");
            return Ok((0, 0));
        }
    };

    // 6c. Ensure file identity + file occurrence records.
    let file_occurrence_id = ensure_file_occurrence(
        conn,
        repo_relative,
        repository_id,
        source_revision_id,
        index_generation_id,
    )?;

    // 6d. Delete existing units if refresh mode is on.
    let deleted = if opts.refresh_existing {
        delete_retrieval_units_for_file(conn, &file_occurrence_id)?
    } else {
        0
    };

    // 6e. Build AnalyzerInput.
    let file_type = infer_file_type(abs_path);
    let content_bytes: Vec<u8> = content.into_bytes();
    let size_bytes = content_bytes.len() as u64;
    let budget = ResourceBudget {
        max_retrieval_units: opts.max_units_per_file as u64,
        ..Default::default()
    };

    // Parse file_occurrence_id string back to the typed wrapper.
    let file_occ_id: FileOccurrenceId = file_occurrence_id
        .parse()
        .map_err(|_| IndexError::Io {
            path: repo_relative.to_owned(),
            source: std::io::Error::other("invalid file_occurrence_id UUID"),
        })?;

    let input = AnalyzerInput {
        file_occurrence_id: file_occ_id,
        path: abs_path.to_path_buf(),
        content: attic_analyzers::AnalyzerContent::FullBytes(content_bytes),
        file_type,
        language_hint: None,
        size_bytes,
        is_partial_scan: false,
        cancellation_token: CancellationToken::default(),
        resource_budget: budget,
    };

    // 6f. Run the analyzer.
    let output = attic_analyzers::dispatch(&registry, input);

    // 6g. Persist retrieval units.
    let analyzer_id = output.analyzer_id.as_str();
    let analyzer_version = output.analyzer_version.as_str();
    let mut inserted = 0usize;

    for unit_spec in &output.retrieval_units {
        let unit_id = RetrievalUnitId::new_v4().to_string_repr();
        let retrieval_text = if is_redacted {
            "[REDACTED]".to_owned()
        } else {
            unit_spec.retrieval_text.clone()
        };

        let unit = NewRetrievalUnit {
            id: &unit_id,
            file_occurrence_id: &file_occurrence_id,
            index_generation_id,
            repository_id,
            retrieval_text: &retrieval_text,
            analyzer_id,
            analyzer_version,
            start_line: Some(unit_spec.span.start_line as u32),
            end_line: Some(unit_spec.span.end_line as u32),
            is_redacted,
        };

        insert_retrieval_unit_with_fts(conn, &unit)?;
        inserted += 1;
    }

    debug!(
        path = %repo_relative,
        inserted,
        deleted,
        "file indexed"
    );

    Ok((inserted, deleted))
}

// ---------------------------------------------------------------------------
// Storage bootstrap helpers
// ---------------------------------------------------------------------------

/// Return the UUID string for the repository, creating a new record if absent.
///
/// Uses the canonical root path as the unique key.
fn ensure_repository(
    conn: &Connection,
    root: &Path,
    name: &str,
) -> Result<String, IndexError> {
    let root_str = root.to_string_lossy();

    // Check existing.
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM core_repositories WHERE root_path = ?1 LIMIT 1",
            rusqlite::params![root_str],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Insert new.  Schema: (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at)
    let id = RepositoryId::new_v4().to_string_repr();
    let now_us = now_microseconds();
    conn.execute(
        "INSERT INTO core_repositories
             (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, 1, ?4, ?4)",
        rusqlite::params![id, root_str, name, now_us],
    )?;

    Ok(id)
}

/// Create a new index generation record; return `(gen_id, source_rev_id)`.
///
/// Phase 1D uses stub values for the many versioning columns that will be
/// properly populated in Phase 2.
fn create_index_generation(
    conn: &Connection,
    repository_id: &str,
) -> Result<(String, String), IndexError> {
    let now_us = now_microseconds();

    // Ensure a stub source revision exists for this repository.
    // Schema: (id, repository_id, commit_sha, branch, working_tree_manifest_hash,
    //          discovery_policy_hash, unstable_capture, captured_at)
    let source_rev_id = SourceRevisionId::new_v4().to_string_repr();
    conn.execute(
        "INSERT OR IGNORE INTO core_source_revisions
             (id, repository_id, commit_sha, branch,
              working_tree_manifest_hash, discovery_policy_hash,
              unstable_capture, captured_at)
         VALUES (?1, ?2, NULL, NULL, 'HEAD', 'none', 0, ?3)",
        rusqlite::params![source_rev_id, repository_id, now_us],
    )?;

    // Retrieve the (possibly pre-existing) revision.
    let rev_id: String = conn.query_row(
        "SELECT id FROM core_source_revisions WHERE repository_id = ?1 LIMIT 1",
        rusqlite::params![repository_id],
        |r| r.get(0),
    )?;

    // Insert the index generation record with stub versioning values.
    // Schema: (id, source_revision_id, schema_version, analyzer_registry_version,
    //          analyzer_versions_json, segmentation_version, indexer_version,
    //          discovery_policy_hash, ranking_version, configuration_hash,
    //          secret_detector_version, subsystem_versions_json, created_at)
    let gen_id = IndexGenerationId::new_v4().to_string_repr();
    conn.execute(
        "INSERT INTO core_index_generations
             (id, source_revision_id,
              schema_version, analyzer_registry_version, analyzer_versions_json,
              segmentation_version, indexer_version, discovery_policy_hash,
              ranking_version, configuration_hash,
              secret_detector_version, subsystem_versions_json, created_at)
         VALUES (?1, ?2,
                 '1.0.0', '1.0.0', '{}',
                 '1.0.0', '1.0.0', 'none',
                 '1.0.0', 'none',
                 1, '{}', ?3)",
        rusqlite::params![gen_id, rev_id, now_us],
    )?;

    Ok((gen_id, rev_id))
}

/// Ensure a file identity and file occurrence exist; return the
/// file_occurrence UUID string.
///
/// Schema for `core_file_identities`: (id, repository_id, stable_id_basis)
/// Schema for `core_file_occurrences`: (id, file_identity_id, source_revision_id,
///   index_generation_id, path, content_hash, size_bytes, language, file_type,
///   discovery_class, security_state, existence_state)
fn ensure_file_occurrence(
    conn: &Connection,
    repo_relative: &str,
    repository_id: &str,
    source_revision_id: &str,
    index_generation_id: &str,
) -> Result<String, IndexError> {
    // Check for an existing file occurrence for this path in this repository.
    let existing: Option<String> = conn
        .query_row(
            "SELECT fo.id
               FROM core_file_occurrences fo
               JOIN core_file_identities fi ON fo.file_identity_id = fi.id
              WHERE fi.repository_id = ?1
                AND fo.path = ?2
              LIMIT 1",
            rusqlite::params![repository_id, repo_relative],
            |r| r.get(0),
        )
        .optional()?;

    if let Some(id) = existing {
        return Ok(id);
    }

    // Create file identity.  stable_id_basis = repo-relative path (Phase 1D stub).
    let fi_id = attic_core::FileIdentityId::new_v4().to_string_repr();
    conn.execute(
        "INSERT INTO core_file_identities
             (id, repository_id, stable_id_basis)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![fi_id, repository_id, repo_relative],
    )?;

    // Create file occurrence with stub values for content_hash / size_bytes.
    let fo_id = FileOccurrenceId::new_v4().to_string_repr();
    conn.execute(
        "INSERT INTO core_file_occurrences
             (id, file_identity_id, source_revision_id, index_generation_id,
              path, content_hash, size_bytes, language, file_type,
              discovery_class, security_state, existence_state)
         VALUES (?1, ?2, ?3, ?4,
                 ?5, 'blake3:0', 0, NULL, 'OTHER',
                 'NORMAL', 'clean', 'present')",
        rusqlite::params![fo_id, fi_id, source_revision_id, index_generation_id, repo_relative],
    )?;

    Ok(fo_id)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_microseconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

fn infer_file_type(path: &Path) -> FileType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => FileType::Rust,
        Some("ts") | Some("tsx") => FileType::TypeScript,
        Some("js") | Some("mjs") | Some("cjs") => FileType::JavaScript,
        Some("py") => FileType::Python,
        Some("md") => FileType::Markdown,
        Some("toml") => FileType::Toml,
        Some("json") => FileType::Json,
        Some("yaml") | Some("yml") => FileType::Yaml,
        Some("html") | Some("htm") => FileType::Other,
        Some("css") => FileType::Other,
        _ => FileType::Other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use attic_discovery::DiscoveryPolicy;
    use attic_storage::run_migrations;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        attic_storage::connection::configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn index_empty_directory_succeeds() {
        let tmp = TempDir::new().unwrap();
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts)
            .expect("indexing empty dir should succeed");
        assert_eq!(result.files_indexed, 0);
        assert_eq!(result.units_inserted, 0);
    }

    #[test]
    fn index_single_text_file_inserts_units() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hello.rs",
            "fn main() { println!(\"hello world\"); }\n",
        );

        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts)
            .expect("indexing should succeed");

        assert!(result.files_indexed >= 1, "at least one file indexed");
        assert!(result.units_inserted >= 1, "at least one unit inserted");
        assert!(!result.repository_id.is_empty());
        assert!(!result.index_generation_id.is_empty());
    }

    #[test]
    fn second_index_run_refreshes_units() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "foo.rs", "fn foo() {}\n");

        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions { refresh_existing: true, ..Default::default() };

        let r1 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        // Second run should have deleted the first run's units and re-inserted.
        assert_eq!(r2.units_deleted, r1.units_inserted);
        assert_eq!(r2.units_inserted, r1.units_inserted);
    }

    #[test]
    fn repository_id_is_stable_across_runs() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn a() {}\n");

        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();

        let r1 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        assert_eq!(r1.repository_id, r2.repository_id, "same repository_id on re-index");
    }
}
