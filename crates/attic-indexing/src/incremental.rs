//! Phase 2 — scoped incremental republication over the approved pipeline.
//!
//! A normal one-file edit must NOT trigger a full-workspace reindex.  This
//! module reuses the Phase 1B security layer
//! ([`attic_discovery::preprocess_file_content`] via
//! [`crate::analyze_single_file`]), Phase 1C analyzer dispatch, and the Phase
//! 1A coordinated publication ([`submit_index_publication`]) — there is no
//! second indexing architecture.
//!
//! Scoping rules:
//! - only files named by the verified [`ScopedChanges`] are analyzed;
//! - unchanged files contribute their previously **verified** BLAKE3 hashes to
//!   the new working-tree manifest (canonical change detection never depends
//!   on timestamps alone);
//! - the whole run publishes as ONE coordinated writer-queue mutation, so a
//!   crash mid-publication leaves either the previous coherent state or the
//!   new one (recovery contract CP-04).
//!
//! Rename handling follows ADR-009: path-basis identities stay authoritative;
//! an identical-content move records an explicit HEURISTIC `CONTENT_MATCH`
//! link row and never mutates identity rows.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;

use tracing::debug;

use attic_core::{
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType, IndexGenerationId,
    RepositoryId, SecurityState, SourceRevisionId,
};
use attic_discovery::{DiscoveryPolicy, manifest_hash_from_pairs};
use attic_storage::{
    IndexPublication, OccurrenceSnapshot, PublicationFile, PublicationOccurrence,
    PublicationRetrievalUnit, current_path_hashes_for_repository, insert_identity_link,
    lookup_file_identity_by_basis, lookup_latest_file_occurrence_for_path,
    lookup_occurrence_snapshot, lookup_repository_by_root_path, submit_index_publication,
};

use crate::{
    FileRecord, IndexError, IndexOptions, IndexingStore, PendingUnit, analyze_single_file,
    infer_file_type,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Verified filesystem deltas for one repository (repo-relative paths).
///
/// "Verified" means the caller already confirmed actual source state
/// (existence + BLAKE3 content hash); watcher hints are never passed raw.
#[derive(Debug, Clone, Default)]
pub struct ScopedChanges {
    /// Paths added or modified (content exists on disk right now).
    pub upserts: Vec<String>,
    /// Paths deleted from disk.
    pub deletes: Vec<String>,
    /// Observed `(prior_path, new_path)` rename hints; used ONLY for identity
    /// link records — correctness never depends on them.
    pub rename_hints: Vec<(String, String)>,
}

impl ScopedChanges {
    /// All paths referenced anywhere in this change set.
    pub fn touched_paths(&self) -> BTreeSet<String> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        set.extend(self.upserts.iter().cloned());
        set.extend(self.deletes.iter().cloned());
        set.extend(self.rename_hints.iter().map(|(f, _)| f.clone()));
        set
    }
}

/// Counters from one scoped incremental publication.
#[derive(Debug, Default, Clone)]
pub struct ScopedIndexResult {
    /// Repository UUID string.
    pub repository_id: String,
    /// New SourceRevision UUID string.
    pub source_revision_id: String,
    /// New IndexGeneration UUID string.
    pub index_generation_id: String,
    /// Files republished (added or modified).
    pub files_published: usize,
    /// Files published as DELETED tombstones.
    pub files_deleted: usize,
    /// Old retrieval units removed (FTS synchronised in the same transaction).
    pub units_deleted: usize,
    /// New retrieval units inserted.
    pub units_inserted: usize,
}

/// Stream-hash one file with BLAKE3 (64 KiB chunks; content-only identity).
fn hash_file_content(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Reindex exactly the files named by `changes` and publish atomically.
///
/// The target database MUST already be migrated and the repository MUST have
/// been bootstrapped by a previous full [`crate::index_repository`] run —
/// incremental work never creates the repository row itself.
pub fn index_changes(
    store: &IndexingStore<'_>,
    root: &Path,
    policy: &DiscoveryPolicy,
    opts: &IndexOptions,
    changes: &ScopedChanges,
) -> Result<ScopedIndexResult, IndexError> {
    let root_str = root.to_string_lossy().to_string();
    let repo_id: RepositoryId = store
        .readers
        .with_reader(|c| lookup_repository_by_root_path(c, &root_str))
        .map_err(IndexError::Storage)?
        .ok_or_else(|| IndexError::RepositoryNotBootstrapped(root_str.clone()))?;

    // ── 1. Trusted baseline: verified CURRENT hashes from committed state ──
    let trusted: BTreeMap<String, String> = store
        .readers
        .with_reader(|c| current_path_hashes_for_repository(c, &repo_id))
        .map_err(IndexError::Storage)?
        .into_iter()
        .collect();

    // ── 2. Hash every changed file NOW (canonical detection: content only) ──
    //
    // Three-state rule: ONLY a verified NotFound may become a deletion
    // tombstone.  Permission errors, transient I/O failures, unstable reads
    // and hash failures are propagated as errors so the task retries — never
    // silently converted into "file missing".
    let mut upsert_hashes: BTreeMap<String, String> = BTreeMap::new();
    let mut vanished: Vec<String> = Vec::new();
    for rel in &changes.upserts {
        let abs = root.join(rel);
        if abs.is_dir() {
            // A directory where an indexed file used to be is uncertain, not
            // a clean deletion.
            return Err(IndexError::Io {
                path: rel.clone(),
                source: std::io::Error::other("indexed file path became a directory"),
            });
        }
        match hash_file_content(&abs) {
            Ok(h) => {
                upsert_hashes.insert(rel.clone(), h);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %rel, "upsert target verifiably gone before publication");
                vanished.push(rel.clone());
            }
            Err(source) => {
                // No silent fallback: previous state stays STALE/INVALID and
                // the scheduler retries the task.
                return Err(IndexError::Io {
                    path: rel.clone(),
                    source,
                });
            }
        }
    }

    // ── 3. Compose the incremental manifest pair set ────────────────────────
    let excluded = changes.touched_paths();
    let mut pairs: Vec<(String, String)> = trusted
        .into_iter()
        .filter(|(p, _)| !excluded.contains(p))
        .collect();
    for (p, h) in &upsert_hashes {
        pairs.push((p.clone(), h.clone()));
    }
    for rel in &vanished {
        pairs.retain(|(p, _)| p != rel);
    }
    let manifest_hash = manifest_hash_from_pairs(&pairs);
    let policy_hash = policy
        .hash()
        .map_err(|e| IndexError::PolicyHash(e.to_string()))?;

    // ── 4. Previous occurrences for every touched path ──────────────────────
    let mut old_occurrence_ids: Vec<String> = Vec::new();
    let mut old_snapshots: BTreeMap<String, OccurrenceSnapshot> = BTreeMap::new();
    for path in changes.touched_paths() {
        let snap: Option<OccurrenceSnapshot> = store
            .readers
            .with_reader(|c| lookup_occurrence_snapshot(c, &repo_id, &path))
            .map_err(IndexError::Storage)?;
        if let Some(snap) = snap {
            let latest_id: Option<String> = store
                .readers
                .with_reader(|c| lookup_latest_file_occurrence_for_path(c, &repo_id, &path))
                .map_err(IndexError::Storage)?;
            if latest_id.as_deref() == Some(snap.id.as_str()) {
                old_occurrence_ids.push(snap.id.clone());
            }
            old_snapshots.insert(path, snap);
        }
    }

    // ── 5. Analysis (Phase 1B preprocessing → Phase 1C dispatch), scoped ────
    let registry = if opts.structural {
        crate::structural_pipeline::default_registry()
    } else {
        crate::structural_pipeline::generic_only_registry()
    };
    let rev_id = SourceRevisionId::new_v4();
    let gen_id = IndexGenerationId::new_v4();

    // Phase 3 — known paths = post-pair manifest baseline ∪ upserts.
    let known_paths: BTreeSet<String> = pairs.iter().map(|(p, _)| p.clone()).collect();
    let mut pipeline = crate::structural_pipeline::StructuralPipeline::new(root, known_paths);

    let repo_id_str = repo_id.to_string_repr();
    let mut file_records: Vec<FileRecord> = Vec::new();
    let mut pending_units: Vec<PendingUnit> = Vec::new();

    for rel in &changes.upserts {
        let Some(hash) = upsert_hashes.get(rel) else {
            continue;
        };
        let abs_path = root.join(rel);
        let size_bytes = std::fs::metadata(&abs_path)
            .map(|m| i64::try_from(m.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);

        let rec = FileRecord {
            fi_id: store
                .readers
                .with_reader(|c| lookup_file_identity_by_basis(c, &format!("{repo_id_str}/{rel}")))
                .map_err(IndexError::Storage)?
                .unwrap_or_else(FileIdentityId::new_v4),
            fo_id: FileOccurrenceId::new_v4(),
            stable_id_basis: format!("{repo_id_str}/{rel}"),
            old_fo_id: old_snapshots.get(rel).map(|s| s.id.clone()),
            repo_relative: rel.clone(),
            abs_path,
            content_hash: hash.clone(),
            size_bytes,
            security_state: SecurityState::Pending,
            file_type: infer_file_type(Path::new(rel)),
            is_partial_scan: false,
        };

        match analyze_single_file(&rec, &registry, opts) {
            Ok((mut units, captured)) => {
                pipeline.note_occurrence(rel, &rec.fo_id.to_string_repr());
                if let Some(captured) = captured {
                    pipeline.record(captured);
                }
                pending_units.append(&mut units);
                file_records.push(rec);
            }
            Err(e) => {
                // No silent fallback: previous state stays STALE/INVALID and
                // the failure surfaces to the scheduler for retry.
                return Err(e);
            }
        }
    }

    // ── 6. Publication payload ──────────────────────────────────────────────
    let publication_files: Vec<PublicationFile> = file_records
        .iter()
        .map(|rec| PublicationFile {
            identity_id: rec.fi_id,
            identity_repository_id: repo_id,
            stable_id_basis: rec.stable_id_basis.clone(),
            occurrence: PublicationOccurrence {
                id: rec.fo_id,
                source_revision_id: rev_id,
                index_generation_id: Some(gen_id),
                path: rec.repo_relative.clone(),
                content_hash: rec.content_hash.clone(),
                size_bytes: rec.size_bytes,
                language: None,
                file_type: rec.file_type,
                discovery_class: DiscoveryClass::Vcs,
                security_state: rec.security_state,
                existence_state: ExistenceState::Present,
            },
        })
        .collect();

    let mut tombstones: Vec<PublicationFile> = Vec::new();
    for rel in changes.deletes.iter().chain(vanished.iter()) {
        let snap = old_snapshots.get(rel);
        let identity_id: FileIdentityId = snap
            .and_then(|s| s.file_identity_id.parse().ok())
            .unwrap_or_else(FileIdentityId::new_v4);
        tombstones.push(PublicationFile {
            identity_id,
            identity_repository_id: repo_id,
            stable_id_basis: format!("{repo_id_str}/{rel}"),
            occurrence: PublicationOccurrence {
                id: FileOccurrenceId::new_v4(),
                source_revision_id: rev_id,
                index_generation_id: Some(gen_id),
                path: rel.clone(),
                content_hash: snap.map(|s| s.content_hash.clone()).unwrap_or_default(),
                size_bytes: 0,
                language: None,
                file_type: FileType::Other,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Skipped,
                existence_state: ExistenceState::Deleted,
            },
        });
    }

    let gen_id_str = gen_id.to_string_repr();
    let mut retrieval_units: Vec<PublicationRetrievalUnit> = Vec::new();
    let mut unit_links_by_occ: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for u in pending_units {
        let unit_id = attic_core::RetrievalUnitId::new_v4().to_string_repr();
        if let Some(idx) = u.structural_node_index {
            unit_links_by_occ
                .entry(u.file_occurrence_id.clone())
                .or_default()
                .push((unit_id.clone(), idx));
        }
        retrieval_units.push(PublicationRetrievalUnit {
            id: unit_id,
            file_occurrence_id: u.file_occurrence_id,
            index_generation_id: gen_id_str.clone(),
            repository_id: repo_id_str.clone(),
            retrieval_text: u.retrieval_text,
            analyzer_id: u.analyzer_id,
            analyzer_version: u.analyzer_version,
            start_line: u.start_line,
            end_line: u.end_line,
            is_redacted: u.is_redacted,
        });
    }

    let deps = crate::structural_pipeline::ResolverDeps {
        symbol_definition: &|qname, kinds| {
            store
                .readers
                .with_reader(|c| {
                    attic_storage::lookup_symbol_definition_occurrence(
                        c,
                        &repo_id_str,
                        qname,
                        kinds,
                    )
                })
                .ok()
                .flatten()
        },
        path_occurrence: &|rel_path| {
            store
                .readers
                .with_reader(|c| lookup_latest_file_occurrence_for_path(c, &repo_id, rel_path))
                .ok()
                .flatten()
        },
    };
    let structural_files = pipeline.finish(&deps, &unit_links_by_occ);

    let mut files = publication_files;
    files.extend(tombstones);

    // Tombstone rows must never advertise CURRENT freshness (contract:
    // deleted state is never exposed as CURRENT); they are flipped to
    // INVALID right after publication.
    let tombstone_occ_ids: Vec<String> = files
        .iter()
        .filter(|f| f.occurrence.existence_state == ExistenceState::Deleted)
        .map(|f| f.occurrence.id.to_string_repr())
        .collect();

    // ── 7. ONE coordinated mutation (atomic with FTS synchronisation) ───────
    let stats = submit_index_publication(
        store.writer,
        IndexPublication {
            repository_id: repo_id,
            repository_upsert: None,
            source_revision_id: rev_id,
            commit_sha: None,
            working_tree_manifest_hash: manifest_hash,
            discovery_policy_hash: policy_hash,
            unstable_capture: false,
            index_generation_id: gen_id,
            secret_detector_version: attic_core::constants::SECRET_PATTERN_VERSION,
            subsystem_versions: {
                let mut sv = attic_core::SubsystemVersions::new();
                sv.set(
                    attic_core::constants::subsystem_keys::SCHEMA,
                    attic_core::constants::CURRENT_SCHEMA_VERSION,
                );
                sv.set(
                    attic_core::constants::subsystem_keys::INDEXER,
                    env!("CARGO_PKG_VERSION"),
                );
                sv.set(
                    attic_core::constants::subsystem_keys::ANALYZER_REGISTRY,
                    attic_core::constants::ANALYZER_REGISTRY_VERSION,
                );
                sv.set(
                    attic_core::constants::subsystem_keys::SECRET_DETECTOR,
                    attic_core::constants::SECRET_PATTERN_VERSION.to_string(),
                );
                sv
            },
            files,
            delete_units_for_occurrences: old_occurrence_ids.clone(),
            close_audit_for_occurrences: old_occurrence_ids.clone(),
            retrieval_units,
            structural_files,
            // Replacement semantics: wipe prior structural artifacts of every
            // refreshed occurrence (and of tombstoned files) in-txn.
            delete_structural_for_occurrences: old_occurrence_ids,
        },
    )
    .map_err(IndexError::Storage)?;

    // ── 8. Identity links for identical-content renames (ADR-009) ───────────
    // Runs AFTER the successful publication so links only ever reference
    // committed identities.
    for (from, to) in &changes.rename_hints {
        let (Some(from_snap), Some(new_hash)) = (
            old_snapshots.get(from.as_str()),
            upsert_hashes.get(to.as_str()),
        ) else {
            continue;
        };
        if from_snap.content_hash != *new_hash || from_snap.existence_state == "DELETED" {
            continue;
        }
        let (Ok(from_identity), Some(new_identity)) = (
            from_snap.file_identity_id.parse::<FileIdentityId>(),
            file_records
                .iter()
                .find(|r| &r.repo_relative == to)
                .map(|r| r.fi_id),
        ) else {
            continue;
        };
        let link_id = uuid::Uuid::new_v4().to_string();
        let prior_path = from.clone();
        let new_path = to.clone();
        store
            .writer
            .send(move |conn| {
                let link = attic_storage::NewIdentityLink {
                    id: &link_id,
                    repository_id: &repo_id,
                    from_identity_id: &from_identity,
                    to_identity_id: &new_identity,
                    prior_path: &prior_path,
                    new_path: &new_path,
                    confidence: attic_storage::identity_confidence::HEURISTIC,
                    basis: attic_storage::identity_basis::CONTENT_MATCH,
                    created_at: now_micros(),
                };
                insert_identity_link(conn, &link)
                    .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
            })
            .map_err(IndexError::Storage)?;
    }

    // ── 9. Tombstone occurrences must never advertise CURRENT freshness ────
    // (audit-record closure for replaced artifacts already happened inside
    // the publication transaction, while the old rows still existed.)
    if !tombstone_occ_ids.is_empty() {
        let tomb = tombstone_occ_ids.clone();
        store
            .writer
            .send(move |conn| {
                for t in &tomb {
                    conn.execute(
                        "UPDATE core_file_occurrences
                            SET freshness_state = 'INVALID'
                          WHERE id = ?1 AND existence_state = 'deleted'",
                        [t],
                    )
                    .map_err(attic_storage::StorageError::from)?;
                }
                Ok(())
            })
            .map_err(IndexError::Storage)?;
    }

    let deleted_count = changes.deletes.len() + vanished.len();
    debug!(
        revision = %rev_id,
        generation = %gen_id,
        published = stats.files_published - deleted_count,
        deleted_count,
        units_deleted = stats.units_deleted,
        units_inserted = stats.units_inserted,
        "scoped incremental publication committed"
    );

    Ok(ScopedIndexResult {
        repository_id: repo_id.to_string_repr(),
        source_revision_id: rev_id.to_string_repr(),
        index_generation_id: gen_id.to_string_repr(),
        files_published: stats.files_published - deleted_count,
        files_deleted: deleted_count,
        units_deleted: stats.units_deleted,
        units_inserted: stats.units_inserted,
    })
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
