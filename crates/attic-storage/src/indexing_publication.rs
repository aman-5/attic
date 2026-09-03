//! Coordinated indexing publication service (Phase 1D).
//!
//! Routes **every** mutation of one indexing run through a single
//! [`WriterQueueHandle::send`] submission — the approved Phase 1A coordinated
//! writer path.  The submitted closure executes on the dedicated writer thread
//! inside the ambient `BEGIN IMMEDIATE … COMMIT` transaction that
//! `WriterQueue` opens for each batch, so all primitives invoked here are the
//! transaction-assuming variants that never open their own transactions.
//!
//! # Nested-transaction safety
//!
//! `publish_file_batch` and `run_migrations` open their own `BEGIN IMMEDIATE`.
//! They MUST NOT be called from inside a writer-queue closure.  This module
//! composes the plain statement primitives instead (`upsert_file_identity`,
//! `insert_file_occurrence`, `insert_retrieval_unit_with_fts`, …) which are
//! safe to run inside an ambient transaction.  The whole publication is
//! therefore atomic: either every write of one indexing run commits together,
//! or the batch is rolled back and no partial state survives.
//!
//! # Failure semantics
//!
//! If any primitive fails, the closure returns `Err` and `WriterQueue` rolls
//! back the batch; the original error is propagated to this function's caller.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use attic_core::{
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType, FreshnessState,
    IndexGenerationId, RepositoryId, SecurityState, SourceRevisionId, SubsystemVersions,
};

use crate::error::StorageError;
use crate::fts::{delete_retrieval_units_for_file, insert_retrieval_unit_with_fts};
use crate::repository::file_occurrence::{
    insert_file_occurrence_with_freshness, upsert_file_identity,
};
use crate::repository::index_generation::insert_index_generation;
use crate::repository::repository::upsert_repository;
use crate::repository::source_revision::insert_source_revision_with_hashes;
use crate::writer::WriterQueueHandle;

// ---------------------------------------------------------------------------
// Owned payload types
// ---------------------------------------------------------------------------

/// Fully owned file occurrence record for queue submission.
#[derive(Debug, Clone)]
pub struct PublicationOccurrence {
    /// Primary key UUID for this occurrence row.
    pub id: FileOccurrenceId,
    /// Foreign key to `core_source_revisions.id`.
    pub source_revision_id: SourceRevisionId,
    /// Foreign key to `core_index_generations.id`.
    pub index_generation_id: Option<IndexGenerationId>,
    /// Workspace-relative normalized path (forward slashes).
    pub path: String,
    /// BLAKE3 hex digest of the raw file bytes.
    pub content_hash: String,
    /// File size in bytes.
    pub size_bytes: i64,
    /// Detected language; `None` for binary or language-unknown files.
    pub language: Option<String>,
    /// Broad file-type classification.
    pub file_type: FileType,
    /// How this file was discovered.
    pub discovery_class: DiscoveryClass,
    /// Security classification derived from secret scanning.
    pub security_state: SecurityState,
    /// Whether the file is present or deleted in this revision.
    pub existence_state: ExistenceState,
}

/// One file's identity + occurrence pair, mirroring
/// [`crate::repository::publication::PublicationItem`] in owned form.
#[derive(Debug, Clone)]
pub struct PublicationFile {
    /// Stable identity UUID (idempotently upserted).
    pub identity_id: FileIdentityId,
    /// Foreign key to `core_repositories.id` for the identity row.
    pub identity_repository_id: RepositoryId,
    /// Canonical basis string for cross-revision identity.
    pub stable_id_basis: String,
    /// Occurrence record to insert for this run.
    pub occurrence: PublicationOccurrence,
}

/// Fully owned retrieval-unit record for queue submission.
#[derive(Debug, Clone)]
pub struct PublicationRetrievalUnit {
    /// Stable UUID string for this retrieval unit.
    pub id: String,
    /// FK → `core_file_occurrences.id` (UUID string).
    pub file_occurrence_id: String,
    /// FK → `core_index_generations.id` (UUID string).
    pub index_generation_id: String,
    /// FK → `core_repositories.id` (UUID string, denormalized).
    pub repository_id: String,
    /// Safe retrieval text (must not contain secrets).
    pub retrieval_text: String,
    /// Analyzer identifier that produced this unit.
    pub analyzer_id: String,
    /// Analyzer version that produced this unit.
    pub analyzer_version: String,
    /// Start line within the file (0-based).
    pub start_line: Option<u32>,
    /// End line within the file (0-based, inclusive).
    pub end_line: Option<u32>,
    /// Whether this unit's text was redacted by Phase 1B.
    pub is_redacted: bool,
}

/// Everything one indexing run writes, in owned form, ready for a single
/// coordinated submission through the writer queue.
#[derive(Debug, Clone)]
pub struct IndexPublication {
    /// Repository these writes belong to.
    pub repository_id: RepositoryId,
    /// `Some((root_path, display_name))` → upsert the repository row first.
    /// `None` when the caller has already confirmed the row exists.
    pub repository_upsert: Option<(String, String)>,
    /// New source-revision UUID.
    pub source_revision_id: SourceRevisionId,
    /// Git commit SHA, `None` for non-Git repositories.
    pub commit_sha: Option<String>,
    /// 64-char BLAKE3 manifest hash from discovery.
    pub working_tree_manifest_hash: String,
    /// 64-char BLAKE3 hash of the serialised discovery policy.
    pub discovery_policy_hash: String,
    /// Whether the manifest contained unstable captures.
    pub unstable_capture: bool,
    /// New index-generation UUID.
    pub index_generation_id: IndexGenerationId,
    /// Secret-pattern version recorded on the generation.
    pub secret_detector_version: i64,
    /// Real subsystem versions for the generation row.
    pub subsystem_versions: SubsystemVersions,
    /// File identities + occurrences to publish.
    pub files: Vec<PublicationFile>,
    /// Previous-run occurrence IDs whose retrieval units must be deleted
    /// before the new units are inserted (refresh path).
    pub delete_units_for_occurrences: Vec<String>,
    /// Occurrence IDs whose pending `core_invalidation_records` must be
    /// closed atomically BEFORE their derived rows are removed — otherwise
    /// the audit rows could never be resolved afterwards.
    pub close_audit_for_occurrences: Vec<String>,
    /// Retrieval units to insert with FTS synchronisation.
    pub retrieval_units: Vec<PublicationRetrievalUnit>,
    /// Phase 3 — per-file structural payloads (nodes/symbols/relationships,
    /// unit↔node links), keyed per file occurrence.
    pub structural_files: Vec<PublicationStructuralFile>,
    /// Phase 3 — occurrences whose previous structural rows are deleted first
    /// (replacement semantics).
    pub delete_structural_for_occurrences: Vec<String>,
}

/// One canonical structural node, ready for persistence (Phase 3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicationNode {
    /// Parent node index within the same file's `nodes` vec.
    pub parent_index: Option<usize>,
    /// Analyzer-defined type tag (`CLASS`, `METHOD`, …).
    pub node_type: String,
    /// Rename-stable identity hash (BLAKE3 hex).
    pub structural_identity: String,
    /// Canonical span string (`"start_line:start_col-end_line:end_col"`).
    pub span_str: String,
    /// BLAKE3 hex of the node's delivered content bytes.
    pub content_hash: String,
    /// Analyzer-specific structured metadata; never contains secret content.
    pub metadata_json: Option<String>,
}

/// One symbol definition occurrence plus its identity tuple (Phase 3).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublicationSymbolDef {
    /// Language tag (`java`, `python`, …) for `core_symbol_identities`.
    pub language: String,
    /// Fully qualified name (language-specific format).
    pub qualified_name: String,
    /// `SymbolKind` DB token.
    pub kind: String,
    /// Overload disambiguator when `(repo, language, qualified_name, kind)`
    /// is ambiguous; `None` otherwise.
    pub disambiguator: Option<String>,
    /// Canonical span string of the definition/signature.
    pub span_str: String,
    /// Language-specific signature text, when extractable.
    pub signature: Option<String>,
    /// Raw visibility modifier, when present.
    pub visibility: Option<String>,
    /// `false` for pure API signatures (no body).
    pub is_definition: bool,
}

/// One relationship edge (Phase 3). Resolution honesty is preserved verbatim
/// from the analyzer/resolver — never upgraded silently.
#[derive(Debug, Clone)]
pub struct PublicationRelationship {
    /// `IMPORT | CALL | EXTENDS | IMPLEMENTS | REFERENCES`
    pub rel_type: String,
    /// Resolved edges: real file-occurrence UUID. Unresolved edges: the raw
    /// logical target string (encoded deterministically at insert time).
    pub target_entity_id: String,
    /// `true` → `target_entity_id` is a real entity UUID.
    pub resolved: bool,
    /// `IMPORT | GO_MODULE | PYTHON_PACKAGE | NPM | MAVEN | HEURISTIC`
    pub dependency_basis: String,
    /// `ResolutionLevel` DB token of the FINAL edge after resolution.
    pub resolution: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Index into this file's symbol vec when the edge originates at a
    /// specific symbol; `None` = file-scoped edge.
    pub source_symbol_index: Option<usize>,
    /// Structured provenance (analyzer ids, spans, resolution basis); no
    /// secret content.
    pub provenance_json: Option<String>,
}

/// Links a retrieval unit to one structural node (Phase 3).
#[derive(Debug, Clone)]
pub struct PublicationUnitLink {
    /// Stable UUID string of the already-published retrieval unit.
    pub retrieval_unit_id: String,
    /// Node index within this file's structural payload.
    pub node_index: usize,
    /// Ordering of nodes within this retrieval unit.
    pub ordinal: u32,
}

/// Per-file structural payload for a coordinated publication (Phase 3).
#[derive(Debug, Clone)]
pub struct PublicationStructuralFile {
    /// Target file occurrence (UUID string).
    pub file_occurrence_id: String,
    /// `false` when the analyzer reported PARTIAL structural coverage
    /// (LARGE-file prefix truncation, entity caps, mid-extraction stop).
    /// Persisted on every node row so partial structure can never be
    /// presented as complete.
    pub structurally_complete: bool,
    /// Analyzer that produced this payload.
    pub analyzer_id: String,
    /// Analyzer version.
    pub analyzer_version: String,
    /// Structural nodes in discovery order (parents before children).
    pub nodes: Vec<PublicationNode>,
    /// Symbol definitions found in this file.
    pub symbols: Vec<PublicationSymbolDef>,
    /// Relationship edges originating in this file.
    pub relationships: Vec<PublicationRelationship>,
    /// Retrieval-unit ↔ structural-node associations.
    pub unit_links: Vec<PublicationUnitLink>,
}

/// Counters returned by a successful coordinated publication.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IndexPublicationStats {
    /// Number of file identities/occurrences published.
    pub files_published: usize,
    /// Number of stale retrieval units deleted.
    pub units_deleted: usize,
    /// Number of retrieval units inserted.
    pub units_inserted: usize,
    /// Phase 3 — structural nodes inserted.
    pub structural_nodes: usize,
    /// Phase 3 — symbol definition occurrences inserted.
    pub symbols_inserted: usize,
    /// Phase 3 — relationships inserted.
    pub relationships_inserted: usize,
    /// Phase 3 — unit↔node links inserted.
    pub unit_node_links: usize,
    /// Phase 3 — prior structural rows removed (nodes+symbols+rels).
    pub structural_deleted: usize,
}

// ---------------------------------------------------------------------------
// Executor — runs inside the writer queue's ambient transaction
// ---------------------------------------------------------------------------

/// Execute the publication on `conn`.
///
/// The caller MUST already hold an open transaction (the writer queue provides
/// one).  No primitive used here opens its own transaction.
fn execute_index_publication(
    conn: &Connection,
    p: &IndexPublication,
) -> Result<IndexPublicationStats, StorageError> {
    if let Some((root_path, display_name)) = &p.repository_upsert {
        upsert_repository(conn, &p.repository_id, root_path, display_name)?;
    }

    insert_source_revision_with_hashes(
        conn,
        &p.source_revision_id,
        &p.repository_id,
        p.commit_sha.as_deref(),
        &p.working_tree_manifest_hash,
        &p.discovery_policy_hash,
        p.unstable_capture,
    )?;

    insert_index_generation(
        conn,
        &p.index_generation_id,
        &p.repository_id,
        &p.source_revision_id,
        p.secret_detector_version,
        &p.subsystem_versions,
    )?;

    // Close pending invalidation-audit rows while the old derived rows are
    // still present (they are referenced by the audit records).
    for occ in &p.close_audit_for_occurrences {
        crate::invalidation_ops::close_pending_records_for_occurrence(conn, occ, {
            use std::time::{SystemTime, UNIX_EPOCH};
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as i64)
                .unwrap_or(0)
        })?;
    }

    for f in &p.files {
        upsert_file_identity(
            conn,
            &f.identity_id,
            &f.identity_repository_id,
            &f.stable_id_basis,
        )?;
        let occ = &f.occurrence;
        let occurrence = crate::repository::file_occurrence::NewFileOccurrence {
            id: &occ.id,
            file_identity_id: &f.identity_id,
            source_revision_id: &occ.source_revision_id,
            index_generation_id: occ.index_generation_id.as_ref(),
            path: &occ.path,
            content_hash: &occ.content_hash,
            size_bytes: occ.size_bytes,
            language: occ.language.as_deref(),
            file_type: occ.file_type,
            discovery_class: occ.discovery_class,
            security_state: occ.security_state,
            existence_state: occ.existence_state,
        };
        let freshness = if occ.existence_state == ExistenceState::Deleted {
            FreshnessState::Invalid
        } else {
            FreshnessState::Current
        };
        insert_file_occurrence_with_freshness(conn, &occurrence, freshness)?;
    }

    let mut stats = IndexPublicationStats {
        files_published: p.files.len(),
        ..Default::default()
    };

    // ── Phase 3 FIRST: structural replacement + unit↔node link cleanup.
    // Dependent state (links → relationships → symbol occurrences → leaf
    // nodes) is removed before its parents; see
    // `delete_structural_for_occurrences` for the explicit ordering.
    let deleted_structural = crate::repository::structural::delete_structural_for_occurrences(
        conn,
        &p.delete_structural_for_occurrences,
    )?;
    stats.structural_deleted =
        deleted_structural.0 + deleted_structural.1 + deleted_structural.2 + deleted_structural.3;

    for old_occurrence_id in &p.delete_units_for_occurrences {
        stats.units_deleted += delete_retrieval_units_for_file(conn, old_occurrence_id)?;
    }

    for u in &p.retrieval_units {
        insert_retrieval_unit_with_fts(
            conn,
            &crate::fts::NewRetrievalUnit {
                id: &u.id,
                file_occurrence_id: &u.file_occurrence_id,
                index_generation_id: &u.index_generation_id,
                repository_id: &u.repository_id,
                retrieval_text: &u.retrieval_text,
                analyzer_id: &u.analyzer_id,
                analyzer_version: &u.analyzer_version,
                start_line: u.start_line,
                end_line: u.end_line,
                is_redacted: u.is_redacted,
            },
        )?;
        stats.units_inserted += 1;
    }

    // ── Phase 3: structural payload insertion (deletion already done above).
    let repo_str = p.repository_id.to_string_repr();
    let rev_str = p.source_revision_id.to_string_repr();
    for sf in &p.structural_files {
        let counts =
            crate::repository::structural::insert_structural_file(conn, &repo_str, &rev_str, sf)?;
        stats.structural_nodes += counts.nodes;
        stats.symbols_inserted += counts.symbols;
        stats.relationships_inserted += counts.relationships;
        stats.unit_node_links += counts.links;
    }

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Submit one full indexing publication through the coordinated writer queue.
///
/// The publication executes as a single mutation inside the queue's ambient
/// transaction, so it is atomic with respect to every other writer-queue
/// client and can never nest transactions.  Blocks until the mutation has
/// been committed or rolled back.
///
/// # Errors
///
/// - Any [`StorageError`] raised by the underlying primitives (the batch is
///   rolled back).
/// - [`StorageError::QueueFull`], [`StorageError::QueueShutdown`],
///   [`StorageError::WriterPoisoned`], [`StorageError::BatchRolledBack`] from
///   the queue itself.
pub fn submit_index_publication(
    writer: &WriterQueueHandle,
    publication: IndexPublication,
) -> Result<IndexPublicationStats, StorageError> {
    // Shared slot carrying the executor's stats back out of the 'static
    // closure.  Written exactly once, only on success.  On failure the
    // closure's original error is delivered by `send` itself after the batch
    // rollback, so no error must travel through this slot.
    let slot: Arc<Mutex<Option<IndexPublicationStats>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);

    writer.send(move |conn| {
        let stats = execute_index_publication(conn, &publication)?;
        if let Ok(mut guard) = sink.lock() {
            *guard = Some(stats);
        }
        Ok(())
    })?;

    match slot.lock() {
        Ok(guard) => guard.ok_or_else(|| {
            StorageError::Worker(
                "index publication closure completed without recording stats".into(),
            )
        }),
        Err(_) => Err(StorageError::MutexPoisoned(
            "index publication stats slot poisoned".into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WriterQueue;
    use crate::connection::open_db;
    use crate::migration::run_migrations;
    use attic_core::{DiscoveryClass, ExistenceState, FileType, RetrievalUnitId, SecurityState};
    use tempfile::TempDir;

    fn make_queue(dir: &TempDir) -> (WriterQueue, crate::DbPool, std::path::PathBuf) {
        let db_path = dir.path().join("coordinated.db");
        let (conn, pool) = open_db(&db_path).expect("open_db");
        run_migrations(&conn).expect("migrations");
        let queue = WriterQueue::new(conn).expect("writer queue");
        let _handle = queue.handle();
        (queue, pool, db_path)
    }

    fn sample_publication() -> IndexPublication {
        let repo_id = RepositoryId::new_v4();
        let rev_id = SourceRevisionId::new_v4();
        let gen_id = IndexGenerationId::new_v4();
        let fi_id = FileIdentityId::new_v4();
        let fo_id = FileOccurrenceId::new_v4();

        let files = vec![PublicationFile {
            identity_id: fi_id,
            identity_repository_id: repo_id,
            stable_id_basis: format!("{repo_id}/src/lib.rs"),
            occurrence: PublicationOccurrence {
                id: fo_id,
                source_revision_id: rev_id,
                index_generation_id: Some(gen_id),
                path: "src/lib.rs".into(),
                content_hash: "a".repeat(64),
                size_bytes: 42,
                language: Some("rust".into()),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Clean,
                existence_state: ExistenceState::Present,
            },
        }];
        let units = vec![PublicationRetrievalUnit {
            id: RetrievalUnitId::new_v4().to_string_repr(),
            file_occurrence_id: fo_id.to_string_repr(),
            index_generation_id: gen_id.to_string_repr(),
            repository_id: repo_id.to_string_repr(),
            retrieval_text: "pub fn coordinated_writer_token() {}".into(),
            analyzer_id: "generic".into(),
            analyzer_version: "0.1.0".into(),
            start_line: Some(0),
            end_line: Some(0),
            is_redacted: false,
        }];

        IndexPublication {
            repository_id: repo_id,
            repository_upsert: Some(("/tmp/coordinated-ws".into(), "coord-test".into())),
            source_revision_id: rev_id,
            commit_sha: None,
            working_tree_manifest_hash: "b".repeat(64),
            discovery_policy_hash: "c".repeat(64),
            unstable_capture: false,
            index_generation_id: gen_id,
            secret_detector_version: 2,
            subsystem_versions: SubsystemVersions::default(),
            files,
            delete_units_for_occurrences: vec![],
            close_audit_for_occurrences: vec![],
            retrieval_units: units,
            structural_files: vec![],
            delete_structural_for_occurrences: vec![],
        }
    }

    #[test]
    fn coordinated_publication_commits_through_writer_queue() {
        let dir = TempDir::new().unwrap();
        let (queue, _pool, db_path) = make_queue(&dir);
        let handle = queue.handle();

        let stats = submit_index_publication(&handle, sample_publication()).expect("submission");
        assert_eq!(stats.files_published, 1);
        assert_eq!(stats.units_inserted, 1);
        drop(handle);
        drop(queue);

        // Verify committed rows with an independent connection.
        let verify = crate::connection::open_ro(&db_path).unwrap();
        let unit_count: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(unit_count, 1, "retrieval unit must be visible after commit");

        let fts_hit: i64 = verify
            .query_row(
                "SELECT COUNT(*) FROM fts_retrieval_units WHERE fts_retrieval_units MATCH ?1",
                ["coordinated_writer_token"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_hit, 1, "FTS index must contain the published unit");
    }

    #[test]
    fn failed_publication_rolls_back_everything() {
        let dir = TempDir::new().unwrap();
        let (queue, _pool, db_path) = make_queue(&dir);
        let handle = queue.handle();

        // Duplicate occurrence IDs force a PK violation mid-publication.
        let mut p = sample_publication();
        let dup = p.files[0].occurrence.id;
        let mut second = p.files[0].clone();
        second.identity_id = FileIdentityId::new_v4();
        second.occurrence.id = dup; // duplicate PK
        second.occurrence.path = "src/dup.rs".into();
        p.files.push(second);

        let result = submit_index_publication(&handle, p);
        assert!(result.is_err(), "duplicate occurrence PK must fail");
        drop(handle);
        drop(queue);

        let verify = crate::connection::open_ro(&db_path).unwrap();
        let repos: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_repositories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(repos, 0, "rollback must leave no repository row");
        let occurrences: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_file_occurrences", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(occurrences, 0, "rollback must leave no occurrence rows");
    }

    #[test]
    fn refresh_deletes_stale_units_in_same_transaction() {
        let dir = TempDir::new().unwrap();
        let (queue, _pool, db_path) = make_queue(&dir);
        let handle = queue.handle();

        let p1 = sample_publication();
        let old_fo = p1.files[0].occurrence.id.to_string_repr();
        let repo_id = p1.repository_id;
        submit_index_publication(&handle, p1).expect("first publication");

        // Second run: SAME repository (stable id), new revision/generation/
        // occurrence; deletes the previous run's units.
        let mut p2 = sample_publication();
        p2.repository_id = repo_id;
        p2.files[0].identity_repository_id = repo_id;
        p2.files[0].stable_id_basis = format!("{repo_id}/src/lib.rs");
        p2.retrieval_units[0].repository_id = repo_id.to_string_repr();
        let rev2 = SourceRevisionId::new_v4();
        let gen2 = IndexGenerationId::new_v4();
        let fo2 = FileOccurrenceId::new_v4();
        p2.source_revision_id = rev2;
        p2.index_generation_id = gen2;
        p2.repository_upsert = None; // row already exists from run 1
        p2.files[0].occurrence.id = fo2;
        p2.files[0].occurrence.source_revision_id = rev2;
        p2.files[0].occurrence.index_generation_id = Some(gen2);
        p2.retrieval_units[0].id = RetrievalUnitId::new_v4().to_string_repr();
        p2.retrieval_units[0].file_occurrence_id = fo2.to_string_repr();
        p2.retrieval_units[0].index_generation_id = gen2.to_string_repr();
        p2.delete_units_for_occurrences.push(old_fo);
        let stats = submit_index_publication(&handle, p2).expect("second publication");
        assert_eq!(stats.units_deleted, 1);
        assert_eq!(stats.units_inserted, 1);
        drop(handle);
        drop(queue);

        let verify = crate::connection::open_ro(&db_path).unwrap();
        let unit_count: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(unit_count, 1, "exactly the fresh unit must remain");
    }

    #[test]
    fn deleted_occurrence_is_born_invalid_in_publication_transaction() {
        let dir = TempDir::new().unwrap();
        let (queue, _pool, db_path) = make_queue(&dir);
        let handle = queue.handle();
        let mut p = sample_publication();
        let tombstone_id = p.files[0].occurrence.id.to_string_repr();
        p.files[0].occurrence.existence_state = ExistenceState::Deleted;
        p.retrieval_units.clear();
        submit_index_publication(&handle, p).expect("tombstone publication");
        drop(handle);
        drop(queue);

        let verify = crate::connection::open_ro(&db_path).unwrap();
        let state: (String, String) = verify
            .query_row(
                "SELECT existence_state, freshness_state FROM core_file_occurrences WHERE id=?1",
                [&tombstone_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state.0, "deleted");
        assert_eq!(
            state.1, "INVALID",
            "tombstone must commit directly as INVALID"
        );
    }

    #[test]
    fn failed_next_publication_keeps_previous_repo_map_truth() {
        let dir = TempDir::new().unwrap();
        let (queue, _pool, db_path) = make_queue(&dir);
        let handle = queue.handle();
        let p1 = sample_publication();
        let repo_id = p1.repository_id;
        submit_index_publication(&handle, p1.clone()).expect("baseline publication");

        let mut p2 = sample_publication();
        p2.repository_id = repo_id;
        p2.repository_upsert = None;
        p2.files[0].identity_repository_id = repo_id;
        p2.files[0].occurrence.path = "src/lib.rs".into();
        let dup = p2.files[0].occurrence.clone();
        p2.files.push(PublicationFile {
            identity_id: FileIdentityId::new_v4(),
            identity_repository_id: repo_id,
            stable_id_basis: format!("{repo_id}/src/dup.rs"),
            occurrence: PublicationOccurrence {
                path: "src/dup.rs".into(),
                ..dup
            },
        });
        assert!(submit_index_publication(&handle, p2).is_err());
        drop(handle);
        drop(queue);

        let verify = crate::connection::open_ro(&db_path).unwrap();
        let files =
            crate::repository::file_occurrence::current_files_for_repo_map(&verify, &repo_id, None)
                .unwrap();
        assert_eq!(files.len(), 1, "failed N+1 must leave exactly N visible");
        assert_eq!(files[0].0, "src/lib.rs");
    }
}
