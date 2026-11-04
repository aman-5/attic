//! Metadata-only projection from isolated indexes into the shared catalog.

use std::collections::BTreeSet;

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::{RepositoryStoreLease, StorageError, WriterQueueHandle};

const MAX_PROJECTED_PATHS: usize = 4_005;

struct Projection {
    repository: Vec<Value>,
    revisions: Vec<Vec<Value>>,
    identities: Vec<Vec<Value>>,
    occurrences: Vec<Vec<Value>>,
}

/// Publish selected cross-repository provenance metadata into the catalog.
///
/// `paths` should contain manifest, proto and AEM anchor paths chosen by the
/// caller, with at most 4,005 entries (2,000 each for manifests and protobufs,
/// plus the five root manifest anchors). Root manifest anchors are also included.
/// Only the repository record, the latest revision and revisions needed by
/// selected latest occurrences, their identities, and occurrence metadata are
/// copied. No retrieval units, FTS text, analysis JSON or file contents are read.
///
/// The indexed canonical repository ID must equal the lease's stable ID.
/// The catalog writer applies the projection in its ambient atomic transaction.
/// Existing provenance rows are retained because cross-repository snapshots and
/// edges can still refer to them; callers should prune their snapshot history.
/// Returns the number of selected occurrence rows projected.
pub fn export_repository_catalog(
    lease: &RepositoryStoreLease,
    catalog_writer: &WriterQueueHandle,
    paths: &[String],
) -> Result<usize, StorageError> {
    if paths.len() > MAX_PROJECTED_PATHS {
        return Err(StorageError::Worker(format!(
            "catalog projection accepts at most {MAX_PROJECTED_PATHS} paths"
        )));
    }
    let repository_id = lease.repository_id().to_owned();
    let projection = lease.with_reader(|conn| read_projection(conn, &repository_id, paths))?;
    let count = projection.occurrences.len();
    lease.with_publication_fence(|| {
        catalog_writer.send(move |catalog| {
            catalog.execute(
                "INSERT INTO core_repositories
                    (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                    root_path = excluded.root_path,
                    display_name = excluded.display_name,
                    is_git = excluded.is_git,
                    case_sensitive = excluded.case_sensitive,
                    updated_at = excluded.updated_at",
                params_from_iter(projection.repository.iter()),
            )?;
            for revision in &projection.revisions {
                catalog.execute(
                    "INSERT OR IGNORE INTO core_source_revisions
                        (id, repository_id, commit_sha, branch, working_tree_manifest_hash,
                         discovery_policy_hash, unstable_capture, captured_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params_from_iter(revision.iter()),
                )?;
            }
            for identity in &projection.identities {
                catalog.execute(
                    "INSERT OR IGNORE INTO core_file_identities
                        (id, repository_id, stable_id_basis) VALUES (?1, ?2, ?3)",
                    params_from_iter(identity.iter()),
                )?;
            }
            catalog.execute(
                "UPDATE core_file_occurrences SET existence_state = 'deleted'
                 WHERE file_identity_id IN
                    (SELECT id FROM core_file_identities WHERE repository_id = ?1)",
                params![projection.repository[0]],
            )?;
            for occurrence in &projection.occurrences {
                catalog.execute(
                    "INSERT INTO core_file_occurrences
                        (id, file_identity_id, source_revision_id, path, content_hash,
                         size_bytes, language, file_type, discovery_class, security_state,
                         existence_state, freshness_state, secret_scan_state,
                         secret_pattern_version, last_indexed_at, occurrence_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                             ?12, ?13, ?14, ?15,
                             (SELECT COALESCE(MAX(occurrence_seq), 0) + 1
                                FROM core_file_occurrences))
                     ON CONFLICT(id) DO UPDATE SET
                         freshness_state = excluded.freshness_state,
                         security_state = excluded.security_state,
                         existence_state = excluded.existence_state,
                         secret_scan_state = excluded.secret_scan_state,
                         secret_pattern_version = excluded.secret_pattern_version,
                         last_indexed_at = excluded.last_indexed_at",
                    params_from_iter(occurrence.iter()),
                )?;
            }
            Ok(())
        })
    })?;
    Ok(count)
}

fn read_projection(
    conn: &Connection,
    repository_id: &str,
    paths: &[String],
) -> Result<Projection, StorageError> {
    // Keep every metadata read on the same WAL snapshot.
    conn.execute_batch("BEGIN DEFERRED;")?;
    let result = read_snapshot(conn, repository_id, paths);
    let end = conn.execute_batch("ROLLBACK;");
    match (result, end) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Ok(projection), Ok(())) => Ok(projection),
    }
}

fn values(row: &rusqlite::Row<'_>, count: usize) -> rusqlite::Result<Vec<Value>> {
    (0..count).map(|index| row.get(index)).collect()
}

fn read_snapshot(
    conn: &Connection,
    repository_id: &str,
    paths: &[String],
) -> Result<Projection, StorageError> {
    let repository = conn
        .query_row(
            "SELECT id, root_path, display_name, is_git, case_sensitive,
                    created_at, updated_at FROM core_repositories WHERE id = ?1",
            params![repository_id],
            |row| values(row, 7),
        )
        .optional()?
        .ok_or_else(|| {
            StorageError::Worker(
                "indexed repository ID differs from manager stable ID; bootstrap with the registration stable_id".into(),
            )
        })?;
    let mut revision_ids = BTreeSet::new();
    let latest: Option<String> = conn
        .query_row(
            "SELECT id FROM core_source_revisions WHERE repository_id = ?1
             ORDER BY captured_at DESC, id DESC LIMIT 1",
            params![repository_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(latest) = latest {
        revision_ids.insert(latest);
    }
    let mut selected_paths: BTreeSet<&str> = paths.iter().map(String::as_str).collect();
    selected_paths.extend([
        "go.mod",
        "package.json",
        "pom.xml",
        "pyproject.toml",
        "build.gradle",
    ]);
    let mut projection = Projection {
        repository,
        revisions: Vec::new(),
        identities: Vec::new(),
        occurrences: Vec::new(),
    };
    let mut occurrence_statement = conn.prepare_cached(
        "SELECT fo.id, fo.file_identity_id, fo.source_revision_id, fo.path,
                fo.content_hash, fo.size_bytes, fo.language, fo.file_type,
                fo.discovery_class, fo.security_state, fo.existence_state,
                fo.freshness_state, fo.secret_scan_state, fo.secret_pattern_version,
                fo.last_indexed_at, fi.stable_id_basis
           FROM core_file_occurrences fo
           JOIN core_file_identities fi ON fi.id = fo.file_identity_id
          WHERE fi.repository_id = ?1 AND fo.path = ?2
          ORDER BY fo.occurrence_seq DESC LIMIT 1",
    )?;
    let mut identities = BTreeSet::new();
    for path in selected_paths {
        let occurrence = occurrence_statement
            .query_row(params![repository_id, path], |row| values(row, 16))
            .optional()?;
        if let Some(mut occurrence) = occurrence {
            let basis = occurrence.pop().expect("identity basis column");
            let Value::Text(identity_id) = &occurrence[1] else {
                return Err(StorageError::Worker(
                    "invalid file identity in index".into(),
                ));
            };
            if identities.insert(identity_id.clone()) {
                projection.identities.push(vec![
                    Value::Text(identity_id.clone()),
                    Value::Text(repository_id.to_owned()),
                    basis,
                ]);
            }
            let Value::Text(revision_id) = &occurrence[2] else {
                return Err(StorageError::Worker(
                    "invalid source revision in index".into(),
                ));
            };
            revision_ids.insert(revision_id.clone());
            projection.occurrences.push(occurrence);
        }
    }
    let mut revision_statement = conn.prepare_cached(
        "SELECT id, repository_id, commit_sha, branch, working_tree_manifest_hash,
                discovery_policy_hash, unstable_capture, captured_at
           FROM core_source_revisions WHERE id = ?1 AND repository_id = ?2",
    )?;
    for revision_id in revision_ids {
        projection.revisions.push(
            revision_statement
                .query_row(params![revision_id, repository_id], |row| values(row, 8))?,
        );
    }
    Ok(projection)
}
