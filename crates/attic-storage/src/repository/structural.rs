//! Phase 3 — structural artifact persistence (transaction-assuming
//! primitives). All functions run inside the writer queue's ambient
//! transaction; none opens its own.
//!
//! Replacement semantics: for every refreshed file occurrence the previous
//! structural nodes, symbol occurrences and relationships anchored at it are
//! deleted before the fresh rows are inserted, so stale artifacts can never
//! survive a re-analysis (Phase 3 contract §17: no ghost relationships).
//!
//! Unresolved relationship targets: `core_relationships.target_entity_id`
//! carries NO foreign key by design, so edges whose target could not be
//! resolved to a real entity are stored with
//! `target_entity_type = 'FILE_OCCURRENCE'` and a deterministic logical id
//! `"logical:" <16-hex>` derived from the target string. The full target is
//! preserved in `provenance_json.logical_target` and `resolution` stays
//! `SYNTACTIC`. See ADR-011.

use rusqlite::Connection;

use crate::error::StorageError;
use crate::indexing_publication::PublicationStructuralFile;

/// Delete all structural artifacts anchored at the given occurrences:
/// nodes and symbol occurrences owned by them, plus relationships where the
/// occurrence is the source entity or a resolved file-level target.
pub fn delete_structural_for_occurrences(
    conn: &Connection,
    occurrence_ids: &[String],
) -> Result<(usize, usize, usize, usize), StorageError> {
    let mut links = 0;
    let mut rels = 0;
    let mut symbols = 0;
    let mut nodes = 0;

    // ── 1. Unit↔node links (FK → retrieval_units AND structural_nodes) ─────
    // Dependent state must go before BOTH of its parents.
    {
        let mut stmt = conn.prepare(
            "DELETE FROM core_retrieval_unit_nodes
              WHERE retrieval_unit_id IN (
                    SELECT id FROM core_retrieval_units WHERE file_occurrence_id = ?1)
                 OR structural_node_id IN (
                    SELECT id FROM core_structural_nodes WHERE file_occurrence_id = ?1)",
        )?;
        for id in occurrence_ids {
            links += stmt.execute([id])?;
        }
    }

    // ── 2. Relationships — logical references to symbol/file occurrences;
    //       removed first so no edge outlives its endpoints.
    {
        let mut stmt = conn.prepare(
            "DELETE FROM core_relationships
              WHERE source_entity_id = ?1
                 OR (target_entity_type = 'FILE_OCCURRENCE' AND target_entity_id = ?1)",
        )?;
        for id in occurrence_ids {
            rels += stmt.execute([id])?;
        }
    }

    // ── 3. Symbol occurrences (FK → identities + file occurrences) ─────────
    {
        let mut stmt =
            conn.prepare("DELETE FROM core_symbol_occurrences WHERE file_occurrence_id = ?1")?;
        for id in occurrence_ids {
            symbols += stmt.execute([id])?;
        }
    }

    // ── 4. Structural nodes — leaves first (self-referential parent FK) ────
    //
    // Repeatedly delete only nodes that are nobody's parent (within the
    // whole table; parents and children always share one occurrence).
    // Bounded by row count so a corrupt parent chain fails loudly instead
    // of looping.
    let mut remaining: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT id FROM core_structural_nodes WHERE file_occurrence_id = ?1")?;
        let mut all = Vec::new();
        for id in occurrence_ids {
            let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
            for r in rows {
                all.push(r?);
            }
        }
        all
    };
    let total = remaining.len();
    while !remaining.is_empty() {
        let mut leaf_ids: Vec<String> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id FROM core_structural_nodes n
                  WHERE n.id = ?1
                    AND NOT EXISTS (
                          SELECT 1 FROM core_structural_nodes c
                           WHERE c.parent_id = n.id)",
            )?;
            for id in &remaining {
                if stmt.query_row([id], |_| Ok(())).is_ok() {
                    leaf_ids.push(id.clone());
                }
            }
        }
        if leaf_ids.is_empty() {
            return Err(StorageError::Worker(
                "structural deletion stalled: no leaf nodes among remaining rows \
                 (corrupt parent_id chain)"
                    .into(),
            ));
        }
        {
            let mut del = conn.prepare("DELETE FROM core_structural_nodes WHERE id = ?1")?;
            for id in &leaf_ids {
                nodes += del.execute([id])?;
            }
        }
        let done: std::collections::HashSet<&String> = leaf_ids.iter().collect();
        remaining.retain(|id| !done.contains(id));
    }
    debug_assert_eq!(nodes, total);

    Ok((links, rels, symbols, nodes))
}

/// Insert one file's structural payload. Node parent links are resolved via
/// insertion order (`parent_index` refers to earlier entries in the same vec).
#[allow(clippy::too_many_arguments)]
pub fn insert_structural_file(
    conn: &Connection,
    repository_id: &str,
    source_revision_id: &str,
    sf: &PublicationStructuralFile,
) -> Result<StructuralCounts, StorageError> {
    // ── Structural nodes (two passes: uuids then parents) ──────────────────
    let mut node_ids: Vec<String> = Vec::with_capacity(sf.nodes.len());
    for _ in 0..sf.nodes.len() {
        node_ids.push(uuid::Uuid::new_v4().to_string());
    }
    for (idx, n) in sf.nodes.iter().enumerate() {
        let parent_id = n.parent_index.and_then(|p| node_ids.get(p)).cloned();
        // Merge the partial-analysis marker into per-row metadata so any
        // consumer reading a node sees the completeness verdict directly.
        let metadata_json = merge_partial_marker(&n.metadata_json, sf.structurally_complete);
        conn.execute(
            "INSERT INTO core_structural_nodes
                 (id, repository_id, file_occurrence_id, parent_id, node_type,
                  structural_identity, source_span, content_hash,
                  analyzer_id, analyzer_version, metadata_json, freshness_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'CURRENT')",
            rusqlite::params![
                node_ids[idx],
                repository_id,
                sf.file_occurrence_id,
                parent_id,
                n.node_type,
                n.structural_identity,
                n.span_str,
                n.content_hash,
                sf.analyzer_id,
                sf.analyzer_version,
                metadata_json,
            ],
        )
        .map_err(StorageError::from)?;
    }

    // ── Symbols: identity upsert + definition occurrence ───────────────────
    let mut symbol_ids: Vec<String> = Vec::with_capacity(sf.symbols.len());
    for s in &sf.symbols {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM core_symbol_identities
                  WHERE repository_id = ?1 AND language = ?2 AND qualified_name = ?3
                        AND kind = ?4 AND disambiguator IS ?5",
                rusqlite::params![
                    repository_id,
                    s.language,
                    s.qualified_name,
                    s.kind,
                    s.disambiguator
                ],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let identity_id = match existing {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                conn.execute(
                    "INSERT INTO core_symbol_identities
                         (id, repository_id, language, qualified_name, kind, disambiguator)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        id,
                        repository_id,
                        s.language,
                        s.qualified_name,
                        s.kind,
                        s.disambiguator
                    ],
                )
                .map_err(StorageError::from)?;
                id
            }
        };
        let occ_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO core_symbol_occurrences
                 (id, symbol_identity_id, file_occurrence_id, source_revision_id,
                  source_span, signature, visibility, is_definition)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                occ_id,
                identity_id,
                sf.file_occurrence_id,
                source_revision_id,
                s.span_str,
                s.signature,
                s.visibility,
                s.is_definition,
            ],
        )
        .map_err(StorageError::from)?;
        symbol_ids.push(occ_id);
    }

    // ── Relationships ───────────────────────────────────────────────────────
    let mut rel_count = 0usize;
    for r in &sf.relationships {
        let (source_id, source_type): (String, &'static str) = match r.source_symbol_index {
            Some(i) => match symbol_ids.get(i) {
                Some(id) => (id.clone(), "SYMBOL_OCCURRENCE"),
                None => (sf.file_occurrence_id.clone(), "FILE_OCCURRENCE"),
            },
            None => (sf.file_occurrence_id.clone(), "FILE_OCCURRENCE"),
        };
        let (target_id, target_type): (String, &'static str) = if r.resolved {
            (r.target_entity_id.clone(), "FILE_OCCURRENCE")
        } else {
            (logical_target_id(&r.target_entity_id), "FILE_OCCURRENCE")
        };
        conn.execute(
            "INSERT INTO core_relationships
                 (id, source_repository_id, source_entity_id, source_entity_type,
                  target_repository_id, target_entity_id, target_entity_type,
                  rel_type, dependency_basis, resolution, confidence,
                  provenance_json, source_revision_id, freshness_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     'CURRENT')",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                repository_id,
                source_id,
                source_type,
                repository_id,
                target_id,
                target_type,
                r.rel_type,
                r.dependency_basis,
                r.resolution,
                r.confidence,
                r.provenance_json,
                source_revision_id,
            ],
        )
        .map_err(StorageError::from)?;
        rel_count += 1;
    }

    // ── Retrieval-unit ↔ structural-node links ──────────────────────────────
    let mut link_count = 0usize;
    for l in &sf.unit_links {
        let Some(node_uuid) = node_ids.get(l.node_index) else {
            continue;
        };
        conn.execute(
            "INSERT OR IGNORE INTO core_retrieval_unit_nodes
                 (retrieval_unit_id, structural_node_id, ordinal)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![l.retrieval_unit_id, node_uuid, l.ordinal],
        )
        .map_err(StorageError::from)?;
        link_count += 1;
    }

    Ok(StructuralCounts {
        nodes: sf.nodes.len(),
        symbols: sf.symbols.len(),
        relationships: rel_count,
        links: link_count,
    })
}

/// Deterministic logical-id encoding for unresolved targets (ADR-011).
fn logical_target_id(target: &str) -> String {
    let hex = blake3::hash(target.as_bytes()).to_hex();
    format!("logical:{}", &hex[..32])
}

/// Merge the partial-analysis marker into a node's metadata JSON.
/// Complete files keep metadata unchanged; partial files gain
/// `"structural_partial": true` (existing keys are preserved verbatim).
fn merge_partial_marker(metadata: &Option<String>, complete: bool) -> Option<String> {
    if complete {
        return metadata.clone();
    }
    let mut value = match metadata.as_deref() {
        Some(s) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
        None => serde_json::Value::Object(serde_json::Map::new()),
    };
    if let serde_json::Value::Object(ref mut map) = value {
        map.insert(
            "structural_partial".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    serde_json::to_string(&value)
        .ok()
        .or_else(|| metadata.clone())
}

/// Look up a defining symbol occurrence by qualified name (exact first, then
/// unqualified-suffix match) restricted to the given `SymbolKind` tokens.
/// Returns the newest definition occurrence id.
///
/// Note: suffix matching uses SQL LIKE; identifier-style qualified names
/// never contain `%`/`_` metacharacters beyond `_`, which is treated as a
/// literal by escaping here.
pub fn lookup_symbol_definition_occurrence(
    conn: &Connection,
    repository_id: &str,
    qualified_name: &str,
    kinds: &[&str],
) -> Result<Option<String>, StorageError> {
    let escaped = qualified_name.replace('%', "\\%").replace('_', "\\_");
    for (mode, needle) in [
        (QnameMode::Exact, qualified_name),
        (QnameMode::Suffix, &escaped),
    ] {
        for kind in kinds {
            let sql = match mode {
                QnameMode::Exact => {
                    "SELECT so.id
                       FROM core_symbol_identities si
                       JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
                      WHERE si.repository_id = ?1 AND si.qualified_name = ?2 AND si.kind = ?3
                        AND so.is_definition = 1
                      ORDER BY so.rowid DESC LIMIT 1"
                }
                QnameMode::Suffix => {
                    "SELECT so.id
                       FROM core_symbol_identities si
                       JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
                      WHERE si.repository_id = ?1
                        AND si.qualified_name LIKE '%' || ?2 ESCAPE '\\'
                        AND si.kind = ?3
                        AND so.is_definition = 1
                      ORDER BY so.rowid DESC LIMIT 1"
                }
            };
            let result: Result<String, _> =
                conn.query_row(sql, rusqlite::params![repository_id, needle, kind], |r| {
                    r.get(0)
                });
            match result {
                Ok(id) => return Ok(Some(id)),
                Err(rusqlite::Error::QueryReturnedNoRows) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(None)
}

enum QnameMode {
    Exact,
    Suffix,
}

/// Counters produced while persisting one file's structural payload.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StructuralCounts {
    /// Structural nodes inserted.
    pub nodes: usize,
    /// Symbol occurrences inserted.
    pub symbols: usize,
    /// Relationships inserted.
    pub relationships: usize,
    /// Unit↔node links inserted.
    pub links: usize,
}
