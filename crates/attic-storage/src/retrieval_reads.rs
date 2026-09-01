//! Phase 4 — bounded, read-only retrieval queries.
//!
//! Every function here runs on a reader-pool connection (`&Connection`,
//! opened read-only by [`crate::connection::open_ro`]) and is safe for
//! concurrent use. All SQL is parameterized; every query carries an explicit
//! LIMIT so no unbounded result set can be produced. Freshness filtering
//! follows the evidence contract: rows whose artifact freshness is INVALID
//! are never returned; deleted occurrences are never returned.

use rusqlite::Connection;

use crate::error::StorageError;

/// Maximum rows any single retrieval read may return unless the caller
/// passes a smaller limit (which is then clamped to this ceiling).
pub const MAX_RETRIEVAL_READ_ROWS: usize = 2_000;

fn clamp(limit: usize) -> i64 {
    limit.min(MAX_RETRIEVAL_READ_ROWS) as i64
}

// ---------------------------------------------------------------------------
// Symbol search
// ---------------------------------------------------------------------------

/// One symbol occurrence row joined with its identity and file header.
#[derive(Debug, Clone)]
pub struct SymbolHit {
    /// `core_symbol_identities.id`.
    pub identity_id: String,
    /// `core_symbol_occurrences.id`.
    pub occurrence_id: String,
    /// Fully qualified name.
    pub qualified_name: String,
    /// Symbol kind token (`function`, `class`, ...).
    pub kind: String,
    /// Language of the identity.
    pub language: String,
    /// Repository owning the identity.
    pub repository_id: String,
    /// 1 if this occurrence is a definition.
    pub is_definition: bool,
    /// Span string `start_line:start_col-end_line:end_col`.
    pub span_str: String,
    /// Signature text if captured (never secret-bearing).
    pub signature: Option<String>,
    /// Owning file occurrence.
    pub file_occurrence_id: String,
    /// Repo-relative path of the owning file.
    pub path: String,
    /// Freshness of the *file occurrence* backing this hit
    /// (CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH).
    pub freshness_state: String,
}

/// Search symbol occurrences by exact / suffix / substring name match.
///
/// Deterministic ordering: definitions first, then shorter qualified names,
/// then newest rowid. INVALID-freshness files and deleted occurrences are
/// excluded.
pub fn search_symbols(
    conn: &Connection,
    repository_id: Option<&str>,
    name_fragment: &str,
    limit: usize,
) -> Result<Vec<SymbolHit>, StorageError> {
    // Escape LIKE metacharacters so caller input is matched literally.
    let escaped = name_fragment
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let like = format!("%{escaped}%");
    let sql = "
        SELECT si.id, si.repository_id, si.language, si.qualified_name, si.kind,
               so.id, so.is_definition, so.source_span, so.signature,
               fo.id, fo.path, fo.freshness_state
          FROM core_symbol_identities si
          JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
          JOIN core_file_occurrences  fo ON fo.id  = so.file_occurrence_id
         WHERE (?1 IS NULL OR si.repository_id = ?1)
           AND si.qualified_name LIKE ?2 ESCAPE '\\'
           AND fo.existence_state != 'deleted'
           AND fo.freshness_state IN ('CURRENT', 'STALE', 'UNKNOWN', 'PENDING_REFRESH')
         ORDER BY so.is_definition DESC,
                  LENGTH(si.qualified_name) ASC,
                  so.rowid DESC
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![repository_id, like, clamp(limit)], |r| {
        Ok(SymbolHit {
            identity_id: r.get(0)?,
            repository_id: r.get(1)?,
            language: r.get(2)?,
            qualified_name: r.get(3)?,
            kind: r.get(4)?,
            occurrence_id: r.get(5)?,
            is_definition: r.get::<_, i64>(6)? != 0,
            span_str: r.get(7)?,
            signature: r.get(8)?,
            file_occurrence_id: r.get(9)?,
            path: r.get(10)?,
            freshness_state: r.get(11)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Exact qualified-name lookup (no LIKE), used for definition lookups.
pub fn lookup_symbol_exact(
    conn: &Connection,
    repository_id: Option<&str>,
    qualified_name: &str,
    limit: usize,
) -> Result<Vec<SymbolHit>, StorageError> {
    let sql = "
        SELECT si.id, si.repository_id, si.language, si.qualified_name, si.kind,
               so.id, so.is_definition, so.source_span, so.signature,
               fo.id, fo.path, fo.freshness_state
          FROM core_symbol_identities si
          JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
          JOIN core_file_occurrences  fo ON fo.id  = so.file_occurrence_id
         WHERE (?1 IS NULL OR si.repository_id = ?1)
           AND si.qualified_name = ?2
           AND fo.existence_state != 'deleted'
         ORDER BY so.is_definition DESC, so.rowid DESC
         LIMIT ?3";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        rusqlite::params![repository_id, qualified_name, clamp(limit)],
        |r| {
            Ok(SymbolHit {
                identity_id: r.get(0)?,
                repository_id: r.get(1)?,
                language: r.get(2)?,
                qualified_name: r.get(3)?,
                kind: r.get(4)?,
                occurrence_id: r.get(5)?,
                is_definition: r.get::<_, i64>(6)? != 0,
                span_str: r.get(7)?,
                signature: r.get(8)?,
                file_occurrence_id: r.get(9)?,
                path: r.get(10)?,
                freshness_state: r.get(11)?,
            })
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Relationships
// ---------------------------------------------------------------------------

/// One relationship edge with both endpoint type tags and provenance.
#[derive(Debug, Clone)]
pub struct RelationshipEdge {
    /// Edge UUID.
    pub id: String,
    /// Source endpoint entity UUID (file or symbol occurrence).
    pub source_entity_id: String,
    /// FILE_OCCURRENCE | SYMBOL_OCCURRENCE
    pub source_entity_type: String,
    /// Target endpoint entity UUID (may be logical:<hex> for unresolved).
    pub target_entity_id: String,
    /// FILE_OCCURRENCE | SYMBOL_OCCURRENCE
    pub target_entity_type: String,
    /// IMPORT | CALL | EXTENDS | IMPLEMENTS | DEPENDS_ON | REFERENCES | ...
    pub rel_type: String,
    /// SYNTACTIC | PACKAGE_RESOLVED | SYMBOL_RESOLVED | BUILD_RESOLVED |
    /// FRAMEWORK_RESOLVED | INFERRED
    pub resolution: String,
    /// [0.0, 1.0].
    pub confidence: f64,
    /// Structured provenance JSON (no secret content), when present.
    pub provenance_json: Option<String>,
    /// Revision that produced this edge.
    pub source_revision_id: String,
    /// CURRENT | STALE | INVALID
    pub freshness_state: String,
    /// Repository that owns the source entity.
    pub source_repository_id: String,
    /// Repository that owns the target entity.
    pub target_repository_id: String,
}

/// Fetch CURRENT-or-STALE edges touching `entity_id` in either direction.
///
/// Edges whose freshness is INVALID are excluded; SYNTACTIC edges are
/// included but carry their resolution tag so validation can refuse to count
/// them as resolved facts.
pub fn relationships_for_entity(
    conn: &Connection,
    entity_id: &str,
    limit: usize,
) -> Result<Vec<RelationshipEdge>, StorageError> {
    let sql = "
        SELECT id, source_entity_id, source_entity_type,
               target_entity_id, target_entity_type,
               rel_type, resolution, confidence, provenance_json,
               source_revision_id, freshness_state,
               source_repository_id, target_repository_id
          FROM core_relationships
         WHERE freshness_state IN ('CURRENT', 'STALE')
           AND (source_entity_id = ?1 OR target_entity_id = ?1)
         ORDER BY confidence DESC, id ASC
         LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![entity_id, clamp(limit)], |r| {
        Ok(RelationshipEdge {
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
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Structural nodes
// ---------------------------------------------------------------------------

/// One structural node row.
#[derive(Debug, Clone)]
pub struct NodeRow {
    /// Node UUID.
    /// Node UUID.
    pub id: String,
    /// Parent node UUID; NULL for roots.
    pub parent_id: Option<String>,
    /// Analyzer-defined type string (e.g. `class_declaration`).
    pub node_type: String,
    /// Rename-stable identity hash.
    /// Rename-stable identity hash.
    pub structural_identity: String,
    /// Span string start_line:start_col-end_line:end_col.
    pub span_str: String,
    /// BLAKE3 hex of node content bytes (post-redaction).
    pub content_hash: String,
    /// Analyzer that produced the node.
    pub analyzer_id: String,
    /// Analyzer metadata JSON (no secret content).
    pub metadata_json: Option<String>,
    /// CURRENT | STALE | INVALID | PENDING_REFRESH
    /// CURRENT | STALE | INVALID | PENDING_REFRESH
    pub freshness_state: String,
    /// Owning file occurrence UUID.
    pub file_occurrence_id: String,
    /// Repo-relative path of the owning file.
    pub path: String,
}

fn node_rows_sql(where_clause: &str) -> String {
    format!(
        "SELECT n.id, n.parent_id, n.node_type, n.structural_identity,
                n.source_span, n.content_hash, n.analyzer_id, n.metadata_json,
                n.freshness_state, n.file_occurrence_id, fo.path
           FROM core_structural_nodes n
           JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
          WHERE {where_clause}
          ORDER BY fo.path ASC, n.rowid ASC
          LIMIT %LIMIT%"
    )
}

fn query_nodes(
    conn: &Connection,
    sql_tpl: &str,
    params: &[&dyn rusqlite::ToSql],
    limit: usize,
) -> Result<Vec<NodeRow>, StorageError> {
    let sql = sql_tpl.replace("%LIMIT%", &clamp(limit).to_string());
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params, |r| {
        Ok(NodeRow {
            id: r.get(0)?,
            parent_id: r.get(1)?,
            node_type: r.get(2)?,
            structural_identity: r.get(3)?,
            span_str: r.get(4)?,
            content_hash: r.get(5)?,
            analyzer_id: r.get(6)?,
            metadata_json: r.get(7)?,
            freshness_state: r.get(8)?,
            file_occurrence_id: r.get(9)?,
            path: r.get(10)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Structural outline for one file occurrence (freshness preserved per row).
pub fn structural_nodes_for_file(
    conn: &Connection,
    file_occurrence_id: &str,
    limit: usize,
) -> Result<Vec<NodeRow>, StorageError> {
    query_nodes(
        conn,
        &node_rows_sql("n.file_occurrence_id = ?1 AND n.freshness_state != 'INVALID'"),
        &[&file_occurrence_id],
        limit,
    )
}

/// Nodes by analyzer node-type fragment (e.g. `%class%`) within one repo.
pub fn structural_nodes_by_type(
    conn: &Connection,
    repository_id: &str,
    node_type_like: &str,
    limit: usize,
) -> Result<Vec<NodeRow>, StorageError> {
    query_nodes(
        conn,
        &node_rows_sql(
            "n.repository_id IS NOT NULL AND n.freshness_state != 'INVALID'
             AND n.node_type LIKE ?2 ESCAPE '\\'
             AND n.file_occurrence_id IN (
                 SELECT id FROM core_file_occurrences
                  WHERE repository_id = ?1 AND existence_state != 'deleted')",
        ),
        &[&repository_id, &node_type_like],
        limit,
    )
}

// ---------------------------------------------------------------------------
// File headers
// ---------------------------------------------------------------------------

/// Compact file-occurrence header used for evidence provenance.
#[derive(Debug, Clone)]
pub struct FileHeader {
    /// Occurrence UUID.
    pub id: String,
    /// Owning repository UUID (via the file identity).
    pub repository_id: String,
    /// Workspace-relative normalized path.
    pub path: String,
    /// Raw content BLAKE3 hex (of the indexed revision).
    /// Raw-content BLAKE3 hex at index time.
    pub content_hash: String,
    /// File size in bytes at index time.
    pub size_bytes: i64,
    /// Detected language; NULL for binary/unknown.
    pub language: Option<String>,
    /// Analyzer-neutral file class recorded at index time.
    pub file_type: String,
    /// clean | flagged | pending | skipped
    pub security_state: String,
    /// present | deleted
    pub existence_state: String,
    /// CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH
    /// CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH
    pub freshness_state: String,
    /// Revision that produced this occurrence.
    pub source_revision_id: String,
    /// Generation that indexed it; NULL if not yet indexed.
    pub index_generation_id: Option<String>,
    /// filesystem | vcs | manifest (how the file was discovered)
    pub discovery_class: String,
}

const FILE_HEADER_COLS: &str =
    "fo.id, fi.repository_id, fo.path, fo.content_hash, fo.size_bytes, fo.language,
        fo.file_type, fo.security_state, fo.existence_state, fo.freshness_state,
        fo.source_revision_id, fo.index_generation_id, fo.discovery_class";

const FILE_HEADER_FROM: &str = "
         FROM core_file_occurrences fo
         JOIN core_file_identities fi ON fi.id = fo.file_identity_id";

/// Header for one occurrence id.
pub fn file_header_by_id(
    conn: &Connection,
    occurrence_id: &str,
) -> Result<Option<FileHeader>, StorageError> {
    let sql = format!("SELECT {FILE_HEADER_COLS} {FILE_HEADER_FROM} WHERE fo.id = ?1");
    let result = conn.query_row(&sql, [occurrence_id], header_from_row);
    match result {
        Ok(h) => Ok(Some(h)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Latest (highest stable occurrence sequence) non-deleted occurrence for a repo-relative path.
pub fn latest_occurrence_for_path(
    conn: &Connection,
    repository_id: &str,
    path: &str,
) -> Result<Option<FileHeader>, StorageError> {
    let sql = format!(
        "SELECT {FILE_HEADER_COLS} {FILE_HEADER_FROM}
          WHERE fi.repository_id = ?1 AND fo.path = ?2
          ORDER BY fo.occurrence_seq DESC LIMIT 1"
    );
    let result = conn.query_row(
        &sql,
        rusqlite::params![repository_id, path],
        header_from_row,
    );
    match result {
        Ok(h) => Ok(Some(h)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn header_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FileHeader> {
    Ok(FileHeader {
        id: r.get(0)?,
        repository_id: r.get(1)?,
        path: r.get(2)?,
        content_hash: r.get(3)?,
        size_bytes: r.get(4)?,
        language: r.get(5)?,
        file_type: r.get(6)?,
        security_state: r.get(7)?,
        existence_state: r.get(8)?,
        freshness_state: r.get(9)?,
        source_revision_id: r.get(10)?,
        index_generation_id: r.get(11)?,
        discovery_class: r.get(12)?,
    })
}

// ---------------------------------------------------------------------------
// Retrieval-plan persistence (ops_retrieval_log)
// ---------------------------------------------------------------------------

/// One row of `ops_retrieval_log`. `plan_json` is authoritative; the other
/// columns are projections for efficient querying.
#[derive(Debug, Clone)]
pub struct NewRetrievalPlanRecord {
    /// Plan UUID (primary key).
    pub plan_id: String,
    /// Correlated MCP/tool call UUID.
    pub query_id: String,
    /// Microseconds since Unix epoch (UTC).
    pub created_at_us: i64,
    /// `None` while the plan is still active.
    pub completed_at_us: Option<i64>,
    /// SHA-256 hex of the workspace root path (never the path itself).
    /// SHA-256 hex of the workspace root path (never the path itself).
    pub workspace_id: String,
    /// Classified QueryType string.
    pub query_type: String,
    /// PlanResult string.
    pub result: String,
    /// ConfidenceLevel string.
    pub confidence: String,
    /// AnswerMode string.
    /// AnswerMode string.
    pub policy_mode: String,
    /// Tokens in the assembled context.
    pub context_tokens: i64,
    /// Repair cycles executed.
    pub repair_cycles: i64,
    /// Full serialized RetrievalPlan (authoritative; no secret content).
    pub plan_json: String,
}

/// Persist a retrieval plan record (single INSERT; no partial writes).
/// Must run inside the writer queue's ambient transaction.
pub fn insert_retrieval_plan(
    conn: &Connection,
    rec: &NewRetrievalPlanRecord,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO ops_retrieval_log
             (plan_id, query_id, created_at_us, completed_at_us, workspace_id,
              query_type, result, confidence, policy_mode, context_tokens,
              repair_cycles, plan_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            rec.plan_id,
            rec.query_id,
            rec.created_at_us,
            rec.completed_at_us,
            rec.workspace_id,
            rec.query_type,
            rec.result,
            rec.confidence,
            rec.policy_mode,
            rec.context_tokens,
            rec.repair_cycles,
            rec.plan_json,
        ],
    )
    .map_err(StorageError::from)?;
    Ok(())
}

/// Fetch the stored plan JSON for one plan id.
pub fn get_retrieval_plan_json(
    conn: &Connection,
    plan_id: &str,
) -> Result<Option<String>, StorageError> {
    let result: Result<String, _> = conn.query_row(
        "SELECT plan_json FROM ops_retrieval_log WHERE plan_id = ?1",
        [plan_id],
        |r| r.get(0),
    );
    match result {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
