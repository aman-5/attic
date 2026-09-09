//! Canonical read APIs consumed by the Phase 5 semantic layer (ADR-014).
//!
//! These functions expose ONLY what semantic selection/embedding needs:
//! bounded unit rows with lightweight aggregates, plus a per-unit "anchor"
//! used to rebuild evidence provenance at query time. No vector shapes, no
//! provider concepts — the disposable layer stays replaceable.

use crate::{FileHeader, StorageError, file_header_by_id};
use rusqlite::{Connection, params};

/// One retrieval unit prepared for semantic-unit SELECTION.
#[derive(Debug, Clone)]
pub struct SemanticUnitRow {
    /// `core_retrieval_units.id`.
    pub unit_id: String,
    /// Owning repository UUID.
    pub repository_id: String,
    /// `core_file_occurrences.id` backing this unit.
    pub file_occurrence_id: String,
    /// Generation that produced the unit.
    pub index_generation_id: String,
    /// Pre-redacted searchable text (contract: secrets.md).
    pub retrieval_text: String,
    /// CURRENT | STALE | INVALID
    pub lexical_state: String,
    /// CURRENT | STALE | INVALID | PENDING_REFRESH
    pub freshness_state: String,
    /// True when Phase 1B redacted this unit's text.
    pub is_redacted: bool,
    /// Workspace-relative normalized path.
    pub path: String,
    /// Revision that produced the occurrence.
    pub source_revision_id: String,
    /// Raw-content BLAKE3 hex.
    pub content_hash: String,
    /// SOURCE | CONFIG | DOCUMENT | INFRA | GENERATED | BINARY | UNKNOWN
    pub file_type: String,
    /// IGNORED | LOW_PRIORITY | NORMAL | HIGH_PRIORITY
    pub discovery_class: String,
    /// Microseconds since epoch; None if never indexed.
    pub last_indexed_at_us: Option<i64>,
    /// Structural nodes mapped INTO this unit (structural significance).
    pub unit_node_count: i64,
    /// Definition symbols recorded anywhere in the backing FILE.
    pub file_symbol_defs: i64,
}

const SEMANTIC_UNIT_SQL: &str = r"
SELECT u.id, u.repository_id, u.file_occurrence_id, u.index_generation_id,
       u.retrieval_text, u.lexical_state, u.freshness_state, u.is_redacted,
       o.path, o.source_revision_id, o.content_hash, o.file_type,
       o.discovery_class, o.last_indexed_at,
       (SELECT COUNT(*) FROM core_retrieval_unit_nodes run
          WHERE run.retrieval_unit_id = u.id)                       AS unit_node_count,
       (SELECT COUNT(*) FROM core_symbol_occurrences so
          WHERE so.file_occurrence_id = o.id AND so.is_definition=1) AS file_symbol_defs
  FROM core_retrieval_units   u
  JOIN core_file_occurrences  o ON o.id = u.file_occurrence_id
 WHERE u.lexical_state     = 'CURRENT'
   AND u.freshness_state  != 'INVALID'
   AND o.existence_state  != 'deleted'
   AND o.existence_state  != 'excluded'
   AND o.freshness_state  != 'INVALID'
 ORDER BY u.id ASC
 LIMIT ?1
";

/// Bounded, deterministically ordered stream of selectable units.
pub fn semantic_unit_rows(
    conn: &Connection,
    max_rows: u32,
) -> Result<Vec<SemanticUnitRow>, StorageError> {
    let mut stmt = conn.prepare(SEMANTIC_UNIT_SQL)?;
    let mut rows = stmt.query(params![max_rows as i64])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(SemanticUnitRow {
            unit_id: r.get(0)?,
            repository_id: r.get(1)?,
            file_occurrence_id: r.get(2)?,
            index_generation_id: r.get(3)?,
            retrieval_text: r.get(4)?,
            lexical_state: r.get(5)?,
            freshness_state: r.get(6)?,
            is_redacted: r.get::<_, i64>(7)? != 0,
            path: r.get(8)?,
            source_revision_id: r.get(9)?,
            content_hash: r.get(10)?,
            file_type: r.get(11)?,
            discovery_class: r.get(12)?,
            last_indexed_at_us: r.get(13)?,
            unit_node_count: r.get(14)?,
            file_symbol_defs: r.get(15)?,
        });
    }
    Ok(out)
}

/// The same rows restricted to an explicit id set (enrichment input build).
/// Missing/invalidated ids are simply absent from the result.
pub fn semantic_units_by_ids(
    conn: &Connection,
    ids: &[String],
) -> Result<Vec<SemanticUnitRow>, StorageError> {
    let mut out = Vec::with_capacity(ids.len());
    // Bounded batches keep SQL variable counts safe.
    for chunk in ids.chunks(64) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT u.id, u.repository_id, u.file_occurrence_id, u.index_generation_id,
                    u.retrieval_text, u.lexical_state, u.freshness_state, u.is_redacted,
                    o.path, o.source_revision_id, o.content_hash, o.file_type,
                    o.discovery_class, o.last_indexed_at,
                    (SELECT COUNT(*) FROM core_retrieval_unit_nodes run
                       WHERE run.retrieval_unit_id = u.id),
                    (SELECT COUNT(*) FROM core_symbol_occurrences so
                       WHERE so.file_occurrence_id = o.id AND so.is_definition=1)
               FROM core_retrieval_units   u
               JOIN core_file_occurrences  o ON o.id = u.file_occurrence_id
              WHERE u.id IN ({placeholders})
                AND u.lexical_state = 'CURRENT'
                AND u.freshness_state != 'INVALID'
                AND o.existence_state != 'deleted'
                AND o.existence_state != 'excluded'
                AND o.freshness_state != 'INVALID'
             ORDER BY u.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let paramslice: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(paramslice.as_slice())?;
        while let Some(r) = rows.next()? {
            out.push(SemanticUnitRow {
                unit_id: r.get(0)?,
                repository_id: r.get(1)?,
                file_occurrence_id: r.get(2)?,
                index_generation_id: r.get(3)?,
                retrieval_text: r.get(4)?,
                lexical_state: r.get(5)?,
                freshness_state: r.get(6)?,
                is_redacted: r.get::<_, i64>(7)? != 0,
                path: r.get(8)?,
                source_revision_id: r.get(9)?,
                content_hash: r.get(10)?,
                file_type: r.get(11)?,
                discovery_class: r.get(12)?,
                last_indexed_at_us: r.get(13)?,
                unit_node_count: r.get(14)?,
                file_symbol_defs: r.get(15)?,
            });
        }
    }
    Ok(out)
}

/// Query-time provenance anchor for one semantic unit: everything needed to
/// build contract-valid Evidence (invariant 1) from a kNN hit.
#[derive(Debug, Clone)]
pub struct UnitAnchor {
    /// core_file_occurrences.id backing the unit.
    pub file_occurrence_id: String,
    /// Owning repository UUID.
    pub repository_id: String,
    /// Workspace-relative normalized path.
    pub path: String,
    /// Revision that produced the backing occurrence.
    pub source_revision_id: String,
    /// Generation that indexed it.
    pub index_generation_id: String,
    /// Raw-content BLAKE3 hex at index time.
    pub content_hash: String,
    /// CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH
    pub freshness_state: String,
    /// Reserved for caller-supplied bounded text.
    pub snippet: String,
    /// Inclusive line window derived from mapped structural nodes when
    /// available; `None` columns fall back to whole-file granularity.
    /// Inclusive start line derived from mapped structural nodes.
    pub start_line: Option<u32>,
    /// Inclusive end line derived from mapped structural nodes.
    pub end_line: Option<u32>,
}

fn parse_span_start(s: &str) -> Option<u32> {
    s.split(['-', ':']).next()?.parse().ok()
}
fn parse_span_end(s: &str) -> Option<u32> {
    s.split(['-', ':']).nth(2)?.parse().ok()
}

/// Anchor lookup for one unit id (returns None for invalidated units).
pub fn retrieval_unit_anchor(
    conn: &Connection,
    unit_id: &str,
) -> Result<Option<UnitAnchor>, StorageError> {
    let row: Option<String> = conn
        .prepare("SELECT file_occurrence_id FROM core_retrieval_units WHERE id=?1")?
        .query_row(params![unit_id], |r| r.get::<_, String>(0))
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let Some(fo) = row else { return Ok(None) };
    let Some(header) = file_header_by_id(conn, &fo)? else {
        return Ok(None);
    };
    let fh: FileHeader = header;

    // Span window from the unit's structural nodes (bounded read).
    let mut starts: Vec<u32> = Vec::new();
    let mut ends: Vec<u32> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT sn.source_span FROM core_retrieval_unit_nodes run
               JOIN core_structural_nodes sn ON sn.id = run.structural_node_id
              WHERE run.retrieval_unit_id = ?1 LIMIT 64",
        )?;
        let mut rows = stmt.query(params![unit_id])?;
        while let Some(r) = rows.next()? {
            let span: String = r.get(0)?;
            if let Some(s) = parse_span_start(&span) {
                starts.push(s);
            }
            if let Some(e) = parse_span_end(&span) {
                ends.push(e);
            }
        }
    }

    Ok(Some(UnitAnchor {
        start_line: starts.iter().copied().min(),
        end_line: ends.iter().copied().max(),
        file_occurrence_id: fo,
        repository_id: fh.repository_id,
        path: fh.path,
        source_revision_id: fh.source_revision_id,
        index_generation_id: fh.index_generation_id.unwrap_or_default(),
        content_hash: fh.content_hash,
        freshness_state: fh.freshness_state,
        snippet: String::new(), // filled by caller from stored text if needed
    }))
}

/// Anchor lookup for many unit ids at once. Missing/invalidated ids are
/// simply absent from the result map, same as `retrieval_unit_anchor`
/// returning `None` for them.
///
/// Callers resolving a whole page of kNN hits (hybrid search, semantic
/// selection) used to call `retrieval_unit_anchor` once per hit — 2-3 SQL
/// round trips each, so 100+ hits meant 200-300+ sequential round trips.
/// This batches the identity lookup (unit -> file_occurrence -> repository)
/// into one query per 64-id chunk. The structural-node span scan stays
/// per-unit: it's already bounded to 64 rows per unit via `LIMIT 64`, and a
/// flat `LIMIT` across a batched query would silently truncate later units'
/// span data instead of just capping each unit's own rows.
pub fn retrieval_unit_anchors(
    conn: &Connection,
    unit_ids: &[String],
) -> Result<std::collections::HashMap<String, UnitAnchor>, StorageError> {
    let mut out = std::collections::HashMap::with_capacity(unit_ids.len());
    for chunk in unit_ids.chunks(64) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!(
            "SELECT u.id, u.file_occurrence_id, fi.repository_id, fo.path,
                    fo.source_revision_id, fo.index_generation_id, fo.content_hash,
                    fo.freshness_state
               FROM core_retrieval_units   u
               JOIN core_file_occurrences  fo ON fo.id = u.file_occurrence_id
               JOIN core_file_identities   fi ON fi.id = fo.file_identity_id
              WHERE u.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let paramslice: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let mut rows = stmt.query(paramslice.as_slice())?;
        while let Some(r) = rows.next()? {
            let unit_id: String = r.get(0)?;
            let index_generation_id: Option<String> = r.get(5)?;
            out.insert(
                unit_id,
                UnitAnchor {
                    file_occurrence_id: r.get(1)?,
                    repository_id: r.get(2)?,
                    path: r.get(3)?,
                    source_revision_id: r.get(4)?,
                    index_generation_id: index_generation_id.unwrap_or_default(),
                    content_hash: r.get(6)?,
                    freshness_state: r.get(7)?,
                    snippet: String::new(),
                    start_line: None,
                    end_line: None,
                },
            );
        }
    }

    for (unit_id, anchor) in out.iter_mut() {
        let mut starts: Vec<u32> = Vec::new();
        let mut ends: Vec<u32> = Vec::new();
        let mut stmt = conn.prepare(
            "SELECT sn.source_span FROM core_retrieval_unit_nodes run
               JOIN core_structural_nodes sn ON sn.id = run.structural_node_id
              WHERE run.retrieval_unit_id = ?1 LIMIT 64",
        )?;
        let mut rows = stmt.query(params![unit_id.as_str()])?;
        while let Some(r) = rows.next()? {
            let span: String = r.get(0)?;
            if let Some(s) = parse_span_start(&span) {
                starts.push(s);
            }
            if let Some(e) = parse_span_end(&span) {
                ends.push(e);
            }
        }
        anchor.start_line = starts.iter().copied().min();
        anchor.end_line = ends.iter().copied().max();
    }

    Ok(out)
}
