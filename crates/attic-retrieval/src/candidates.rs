//! Candidate generators (Phase 4 §7): every existing intelligence surface —
//! FTS lexical retrieval, exact/path lookup, Phase 3 symbols, Phase 3
//! structural nodes, imports/references/relationships, project knowledge —
//! becomes a bounded candidate producer. Every candidate retains its origin
//! and raw score; provenance is never discarded during fusion.

use attic_core::FreshnessState;
use attic_evidence::{
    AuthorityLevel, Evidence, EvidenceSourceType, RelationshipProvenance, ResolutionLevel,
    RetrievalSource,
};
use rusqlite::Connection;

use crate::budget::BudgetAccountant;
use crate::error::RetrievalError;

/// Which retriever produced a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetrieverKind {
    Fts,
    Path,
    Symbol,
    Structural,
    Relationship,
    Knowledge,
    /// Phase 6: cross-repository dependency edges.
    CrossRepo,
}

/// Parse a canonical retriever tag back to its kind.
pub fn retriever_from_str(s: &str) -> Option<RetrieverKind> {
    match s {
        "FTS" => Some(RetrieverKind::Fts),
        "PATH" => Some(RetrieverKind::Path),
        "SYMBOL" => Some(RetrieverKind::Symbol),
        "STRUCTURAL" => Some(RetrieverKind::Structural),
        "RELATIONSHIP" | "GRAPH" => Some(RetrieverKind::Relationship),
        "KNOWLEDGE" => Some(RetrieverKind::Knowledge),
        "CROSS_REPO" => Some(RetrieverKind::CrossRepo),
        _ => None,
    }
}

impl RetrieverKind {
    /// Canonical retriever tag recorded in `RetrievalSource`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fts => "FTS",
            Self::Path => "PATH",
            Self::Symbol => "SYMBOL",
            Self::Structural => "STRUCTURAL",
            Self::Relationship => "RELATIONSHIP",
            Self::Knowledge => "KNOWLEDGE",
            Self::CrossRepo => "CROSS_REPO",
        }
    }
}

/// One pre-ranking candidate wrapping its evidence skeleton.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub kind: RetrieverKind,
    pub evidence: Evidence,
}

impl Candidate {
    pub(crate) fn new(kind: RetrieverKind, mut ev: Evidence) -> Self {
        ev.retrieval_sources.push(RetrievalSource {
            retriever_type: kind.as_str().to_owned(),
            score: ev.confidence,
            query_fragment: String::new(),
        });
        Self { kind, evidence: ev }
    }
}

/// Classify an indexed path into the canonical evidence source type
/// (deterministic; documented in ADR-012).
///
/// Only paths under the deliberately curated `knowledge/` boundary receive
/// `Knowledge` (-> `ProjectKnowledge` authority). An ordinary `README.md` or
/// `architecture.md` living anywhere else in the repository is real,
/// searchable documentation evidence (`Documentation` authority) — it must
/// not be silently elevated to the same authority as curated project
/// knowledge just because of its filename.
pub fn source_type_for_path(path: &str) -> EvidenceSourceType {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if lower.starts_with("knowledge/") {
        return EvidenceSourceType::Knowledge;
    }
    // Test markers across supported languages.
    let test_markers = [
        "/tests/",
        "/test/",
        "__tests__/",
        "_test.go",
        "test_",
        "_test.py",
        "test.py",
        "test.java",
        ".test.ts",
        ".test.js",
        ".spec.ts",
        ".spec.js",
        "test.rs",
    ];
    if test_markers.iter().any(|m| lower.contains(m)) || name.ends_with("_test.go") {
        return EvidenceSourceType::Test;
    }
    const CONFIG_EXTS: &[&str] = &[
        ".yml",
        ".yaml",
        ".toml",
        ".ini",
        ".properties",
        ".env",
        ".cfg",
        ".conf",
        ".xml",
    ];
    if CONFIG_EXTS.iter().any(|e| name.ends_with(e)) && !name.starts_with("pom.xml")
        || name == "package.json"
        || name == "go.mod"
    {
        return EvidenceSourceType::Configuration;
    }
    const DOC_EXTS: &[&str] = &[".md", ".rst", ".txt", ".adoc"];
    if DOC_EXTS.iter().any(|e| name.ends_with(e))
        || matches!(
            name,
            "license" | "licence" | "notice" | "copying" | "copyright"
        )
    {
        return EvidenceSourceType::Documentation;
    }
    if name.ends_with(".json") {
        return EvidenceSourceType::Configuration;
    }
    EvidenceSourceType::SourceCode
}

/// Short type label for a `search` result path: `knowledge`,
/// `documentation`, `code`, `config` or `test` (same classification
/// `context` uses).
pub fn search_label_for_path(path: &str) -> &'static str {
    match source_type_for_path(path) {
        EvidenceSourceType::Knowledge => "knowledge",
        EvidenceSourceType::Documentation => "documentation",
        EvidenceSourceType::Configuration => "config",
        EvidenceSourceType::Test => "test",
        EvidenceSourceType::SourceCode
        | EvidenceSourceType::GeneratedSource
        | EvidenceSourceType::Relationship => "code",
    }
}

/// Authority mapping per source type (evidence.md AuthorityLevel).
pub fn authority_for(st: EvidenceSourceType) -> AuthorityLevel {
    match st {
        EvidenceSourceType::SourceCode | EvidenceSourceType::GeneratedSource => {
            AuthorityLevel::Implementation
        }
        EvidenceSourceType::Test => AuthorityLevel::TestExpectation,
        EvidenceSourceType::Configuration => AuthorityLevel::Configured,
        EvidenceSourceType::Knowledge => AuthorityLevel::ProjectKnowledge,
        EvidenceSourceType::Documentation => AuthorityLevel::Doc,
        EvidenceSourceType::Relationship => AuthorityLevel::Derived,
    }
}

/// Parse a stored span string `start_line:start_col-end_line:end_col`.
fn parse_stored_span(s: &str) -> Option<attic_core::SourceSpan> {
    let mut parts = s.split(['-', ':']);
    let sl = parts.next()?.parse::<u32>().ok()?;
    let sc = parts.next()?.parse::<u32>().ok()?;
    let el = parts.next()?.parse::<u32>().ok()?;
    let ec = parts.next()?.parse::<u32>().ok()?;
    Some(attic_core::SourceSpan::new(sl, sc, el, ec))
}

/// Unit line window as a span (columns unknown at unit granularity).
pub fn unit_span(start_line: Option<u32>, end_line: Option<u32>) -> Option<attic_core::SourceSpan> {
    let start_line = start_line?;
    Some(attic_core::SourceSpan::new(
        start_line,
        0,
        end_line.unwrap_or(start_line.saturating_add(1)),
        0,
    ))
}

fn assign_unit_location(ev: &mut Evidence, hit: &attic_storage::FtsSearchResult) -> bool {
    if hit.analyzer_id != "json" {
        ev.source_span = unit_span(hit.start_line, hit.end_line);
        return true;
    }
    ev.indexed_unit_id = Some(hit.retrieval_unit_id.clone());
    let metadata = hit
        .occurrence_metadata
        .as_deref()
        .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok());
    ev.json_pointer = metadata
        .as_ref()
        .and_then(|value| value.get("json_pointer"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    ev.position_basis = metadata
        .as_ref()
        .and_then(|value| value.get("position_basis"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let has_location = metadata.as_ref().is_some_and(|value| {
        value
            .get("source_start_byte")
            .and_then(serde_json::Value::as_u64)
            .is_some()
            && value
                .get("source_end_byte")
                .and_then(serde_json::Value::as_u64)
                .is_some()
            && ev.position_basis.is_some()
    });
    ev.source_span = has_location
        .then(|| unit_span(hit.start_line, hit.end_line))
        .flatten();
    ev.location_status = Some(
        if ev.source_span.is_some() {
            "indexed_unverified"
        } else {
            "unavailable_reindex_required"
        }
        .to_owned(),
    );
    false
}

fn freshness_of(s: &str) -> FreshnessState {
    FreshnessState::from_db_str(s).unwrap_or(FreshnessState::Unknown)
}

/// Fill canonical provenance (revision + generation + content hash) for
/// evidence anchored at a file occurrence. Evidence without a revision id
/// would be rejected by validation (invariant 1), so this MUST run before
/// candidates enter ranking.
pub fn fill_file_provenance(conn: &Connection, ev: &mut Evidence) {
    if let Ok(Some(h)) = attic_storage::file_header_by_id(conn, &ev.source_id) {
        ev.repository_id = h.repository_id.clone();
        ev.path = h.path.clone();
        ev.source_revision_id = Some(h.source_revision_id);
        ev.index_generation_id = h.index_generation_id;
        ev.content_hash = Some(h.content_hash);
        ev.freshness_state = freshness_of(&h.freshness_state);
    }
}

/// Bound a snippet to at most `max_chars` characters on char boundaries.
pub fn bound_snippet(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_owned();
    }
    let mut cut = max_chars;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

/// Keep the existing excerpt budget, centered on the line matching most query terms.
fn focused_snippet(text: &str, terms: &[String], max_bytes: usize) -> (String, usize, usize) {
    let mut offset = 0;
    let mut best = (0, 0, 0);
    for line in text.split_inclusive('\n') {
        let lower = line.to_lowercase();
        let matches = terms
            .iter()
            .filter(|term| lower.contains(term.as_str()))
            .count();
        if matches > best.0 {
            let first = terms
                .iter()
                .filter_map(|term| {
                    let index = lower.find(term.as_str())?;
                    // Unicode lowercasing can change byte lengths (e.g. U+0130).
                    let mut folded_offset = 0;
                    let mut original_start = 0;
                    let mut original_end = line.len();
                    for (original, ch) in line.char_indices() {
                        let next =
                            folded_offset + ch.to_lowercase().map(char::len_utf8).sum::<usize>();
                        if folded_offset <= index && index < next {
                            original_start = original;
                        }
                        if next >= index + term.len() {
                            original_end = original + ch.len_utf8();
                            break;
                        }
                        folded_offset = next;
                    }
                    Some((original_start, original_end - original_start))
                })
                .min_by_key(|(index, _)| *index)
                .unwrap_or((0, 0));
            best = (matches, offset + first.0, first.1);
        }
        offset += line.len();
    }
    let mut start = best.1.saturating_sub(max_bytes / 4);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    // Include a complete leading line when it fits without losing the match.
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    if best.1.saturating_add(best.2).saturating_sub(line_start) <= max_bytes {
        start = line_start;
    }
    let mut end = start.saturating_add(max_bytes).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    let excerpt = &text[start..end];
    (excerpt.to_owned(), start, end)
}

// ---------------------------------------------------------------------------
// Generators (all read-only; each enforces the candidate budget)
// ---------------------------------------------------------------------------

/// Shared inputs all generators need.
pub struct GeneratorEnv<'a> {
    pub conn: &'a Connection,
    /// Repository filter; empty means workspace-wide.
    pub repository_id: Option<String>,
    pub budget: &'a mut BudgetAccountant,
    /// Per-generator result ceiling (clamped to MAX_RETRIEVAL_READ_ROWS).
    pub limit: usize,
}

fn repo_filter<'a>(env: &'a GeneratorEnv<'_>) -> Option<&'a str> {
    env.repository_id.as_deref()
}

/// FTS lexical generator. Query terms are quoted as FTS5 phrases so caller
/// input can never inject MATCH syntax.
pub struct LexicalGenerator;

impl LexicalGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        terms: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_scoped(env, terms, None)
    }

    /// Generate lexical candidates within an optional repository-relative scope.
    pub fn run_scoped(
        env: &mut GeneratorEnv<'_>,
        terms: &[String],
        scope: Option<&attic_storage::PathScope>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // Deterministic phrase construction: `"term1" OR "term2"`.
        let selected = prioritize_lexical_terms(terms);
        let fts_query = selected
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR ");
        let params = attic_storage::FtsSearchParams {
            query: &fts_query,
            repository_id: repo_filter(env),
            file_type: None,
            language: None,
            max_results: if env.budget.candidates_available() {
                env.limit
            } else {
                0
            },
        };
        let hits = attic_storage::fts_search_scoped(env.conn, &params, scope)?;
        let mut out = Vec::new();
        for h in hits {
            if !env.budget.admit_candidate() {
                break;
            }
            let st = source_type_for_path(&h.path);
            let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), h.repository_id.clone());
            ev.source_type = st;
            ev.source_id = h.file_occurrence_id.clone();
            ev.path = h.path.clone();
            ev.freshness_state = freshness_of(&h.freshness_state);
            ev.authority = authority_for(st);
            ev.confidence = attic_evidence::signals::normalize_lexical(h.score.max(0.0));
            let (snippet, start_byte, end_byte) = focused_snippet(&h.body, terms, 1200);
            ev.snippet = Some(snippet);
            ev.snippet_leading_omitted = start_byte > 0;
            ev.snippet_trailing_omitted = end_byte < h.body.len();
            ev.signals.lexical_score = Some(attic_evidence::signals::normalize_lexical(h.score));
            let has_source_excerpt = assign_unit_location(&mut ev, &h);
            ev.excerpt_span = has_source_excerpt
                .then_some(h.start_line)
                .flatten()
                .map(|line| {
                    let before = &h.body[..start_byte];
                    let start =
                        line.saturating_add(
                            before.bytes().filter(|byte| *byte == b'\n').count() as u32
                        );
                    let start_col = before.rsplit('\n').next().unwrap_or("").chars().count() as u32;
                    let excerpt = &h.body[start_byte..end_byte];
                    let end_line = start.saturating_add(
                        excerpt.bytes().filter(|byte| *byte == b'\n').count() as u32,
                    );
                    let end_col = if end_line == start {
                        start_col.saturating_add(excerpt.chars().count() as u32)
                    } else {
                        excerpt.rsplit('\n').next().unwrap_or("").chars().count() as u32
                    };
                    attic_core::SourceSpan::new(start, start_col, end_line, end_col)
                });
            fill_file_provenance(env.conn, &mut ev);
            let mut c = Candidate::new(RetrieverKind::Fts, ev);
            c.evidence.retrieval_sources[0].query_fragment = fts_query.clone();
            out.push(c);
        }
        Ok(out)
    }
}

fn prioritize_lexical_terms(terms: &[String]) -> Vec<&str> {
    let mut ranked: Vec<(usize, &str)> = terms
        .iter()
        .enumerate()
        .map(|(index, term)| (index, term.as_str()))
        .collect();
    ranked.sort_by_key(|(index, term)| {
        let identifier = term.contains('_') || term.chars().any(|c| c.is_ascii_digit());
        let score = usize::from(identifier) * 2 + usize::from(term.len() >= 9);
        (std::cmp::Reverse(score), *index)
    });
    ranked.truncate(24);
    ranked.sort_by_key(|(index, _)| *index);
    ranked.into_iter().map(|(_, term)| term).collect()
}

/// Exact path lookup generator (uses `fts_path_lookup` + header provenance).
pub struct PathExactGenerator;

impl PathExactGenerator {
    pub fn run(env: &mut GeneratorEnv<'_>, path: &str) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_matching(env, path, false)
    }

    pub fn run_inferred(
        env: &mut GeneratorEnv<'_>,
        path: &str,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_matching(env, path, true)
    }

    fn run_matching(
        env: &mut GeneratorEnv<'_>,
        path: &str,
        inferred: bool,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let hits = if inferred {
            attic_storage::fts_path_lookup_inferred(
                env.conn,
                path,
                repo_filter(env),
                env.limit.min(8),
            )?
        } else {
            attic_storage::fts_path_lookup(env.conn, path, repo_filter(env), env.limit.min(64))?
        };
        let mut out = Vec::new();
        for h in hits {
            if !env.budget.admit_candidate() {
                break;
            }
            let st = source_type_for_path(&h.path);
            let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), h.repository_id.clone());
            ev.source_type = st;
            ev.source_id = h.file_occurrence_id.clone();
            ev.path = h.path.clone();
            ev.freshness_state = freshness_of(&h.freshness_state);
            ev.authority = authority_for(ev.source_type);
            ev.confidence = 1.0; // exact path match
            let (snippet, _, end) = focused_snippet(&h.body, &[], 1200);
            ev.snippet = Some(snippet);
            ev.snippet_trailing_omitted = end < h.body.len();
            ev.signals.symbol_match_score = Some(1.0);
            assign_unit_location(&mut ev, &h);
            fill_file_provenance(env.conn, &mut ev);
            out.push(Candidate::new(RetrieverKind::Path, ev));
        }
        Ok(out)
    }
}

/// Symbol generator over the Phase 3 symbol table.
pub struct SymbolGenerator;

impl SymbolGenerator {
    pub fn run(env: &mut GeneratorEnv<'_>, name: &str) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_scoped(env, name, None)
    }

    pub fn run_scoped(
        env: &mut GeneratorEnv<'_>,
        name: &str,
        scope: Option<&attic_storage::PathScope>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        if name.trim().is_empty() {
            return Ok(Vec::new());
        }
        let exact =
            attic_storage::lookup_symbol_exact_scoped(env.conn, repo_filter(env), name, 16, scope)?;
        let exact_match = !exact.is_empty();
        let fuzzy = if exact.is_empty() {
            attic_storage::search_symbols_scoped(
                env.conn,
                repo_filter(env),
                name,
                env.limit.min(64),
                scope,
            )?
        } else {
            Vec::new()
        };
        let mut out = Vec::new();
        for s in exact.into_iter().chain(fuzzy) {
            if !env.budget.admit_candidate() {
                break;
            }
            let st = source_type_for_path(&s.path);
            let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), s.repository_id.clone());
            ev.source_type = st;
            ev.source_id = s.file_occurrence_id.clone();
            ev.path = s.path.clone();
            ev.freshness_state = freshness_of(&s.freshness_state);
            ev.authority = authority_for(ev.source_type);
            ev.confidence = if s.is_definition { 1.0 } else { 0.85 };
            ev.snippet = Some(bound_snippet(
                s.signature.as_deref().unwrap_or(&s.qualified_name),
                400,
            ));
            ev.signals.symbol_match_score = Some(if exact_match { 1.0 } else { 0.7 });
            // Symbol evidence carries the exact occurrence span.
            ev.source_span = parse_stored_span(&s.span_str);
            fill_file_provenance(env.conn, &mut ev);
            let mut c = Candidate::new(RetrieverKind::Symbol, ev);
            c.evidence.retrieval_sources[0].score = c.evidence.confidence;
            out.push(c);
        }
        Ok(out)
    }
}

/// Structural outline generator: outline of files already surfaced by other
/// candidates plus node-type fragments for architecture questions.
pub struct StructuralGenerator;

impl StructuralGenerator {
    pub fn run_for_files(
        env: &mut GeneratorEnv<'_>,
        file_occurrence_ids: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let mut out = Vec::new();
        for fo in file_occurrence_ids.iter().take(8) {
            if !env.budget.candidates_available() {
                break;
            }
            for _n in attic_storage::structural_nodes_for_file(env.conn, fo, 32)?
                .into_iter()
                .filter(|n| {
                    matches!(
                        n.node_type.as_str(),
                        "class_declaration"
                            | "class"
                            | "interface_declaration"
                            | "function_definition"
                            | "method_declaration"
                            | "function_declaration"
                    )
                })
                .take(8)
            {
                if !env.budget.admit_candidate() {
                    break;
                }
                let header = attic_storage::file_header_by_id(env.conn, fo)?;
                let (repo, path, rev) = match &header {
                    Some(h) => (
                        h.repository_id.clone(),
                        h.path.clone(),
                        Some(h.source_revision_id.clone()),
                    ),
                    None => (String::new(), String::new(), None),
                };
                let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), repo);
                ev.source_type = EvidenceSourceType::SourceCode;
                ev.source_id = fo.clone();
                ev.path = path;
                ev.source_revision_id = rev;
                ev.index_generation_id =
                    header.as_ref().and_then(|h| h.index_generation_id.clone());
                ev.content_hash = header.map(|h| h.content_hash);
                ev.authority = AuthorityLevel::Implementation;
                ev.confidence = 0.5;
                ev.signals.structural_proximity = Some(1.0);
                out.push(Candidate::new(RetrieverKind::Structural, ev));
            }
        }
        Ok(out)
    }
}

/// Relationship generator: direct edges of seed entities.
pub struct RelationshipGenerator;

impl RelationshipGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        seed_entity_ids: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_scoped(env, seed_entity_ids, None)
    }

    pub fn run_scoped(
        env: &mut GeneratorEnv<'_>,
        seed_entity_ids: &[String],
        scope: Option<&attic_storage::PathScope>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let mut out = Vec::new();
        for entity in seed_entity_ids.iter().take(16) {
            if !env.budget.candidates_available() {
                break;
            }
            let edges = if let Some(scope) = scope {
                attic_storage::relationships_for_entity_scoped(env.conn, entity, 24, scope)?
                    .into_iter()
                    .map(|(edge, path)| (edge, Some(path)))
                    .collect::<Vec<_>>()
            } else {
                attic_storage::relationships_for_entity(env.conn, entity, 24)?
                    .into_iter()
                    .map(|edge| (edge, None))
                    .collect::<Vec<_>>()
            };
            for (e, scoped_path) in edges {
                if !env.budget.admit_candidate() {
                    break;
                }
                let resolution = ResolutionLevel::from_db_str(&e.resolution)
                    .unwrap_or(ResolutionLevel::Syntactic);
                let mut ev = Evidence::new(
                    uuid::Uuid::new_v4().to_string(),
                    if scoped_path.is_some() {
                        e.source_repository_id.clone()
                    } else {
                        String::new()
                    },
                );
                ev.source_type = EvidenceSourceType::Relationship;
                ev.source_id = e.id.clone();
                let label = relationship_label(&e);
                ev.path = scoped_path.unwrap_or_else(|| label.clone());
                ev.source_revision_id = Some(e.source_revision_id.clone());
                ev.freshness_state = freshness_of(&e.freshness_state);
                ev.authority = AuthorityLevel::Derived;
                ev.confidence = e.confidence.clamp(0.0, 0.99);
                ev.relationship_confidence = Some(e.confidence.clamp(0.0, 0.99));
                ev.relationship = Some(RelationshipProvenance {
                    edge_id: e.id.clone(),
                    rel_type: e.rel_type.clone(),
                    resolution,
                    confidence: e.confidence,
                    hop_depth: 0,
                });
                ev.signals.relationship_confidence = Some(e.confidence);
                // See CrossRepoGenerator: relationship evidence has no
                // source-file snippet, so the edge label must fill that
                // field or context::build drops the item as empty content.
                ev.snippet = Some(label);
                out.push(Candidate::new(RetrieverKind::Relationship, ev));
            }
        }
        Ok(out)
    }
}

fn relationship_label(e: &attic_storage::RelationshipEdge) -> String {
    let unresolved = e.target_entity_id.starts_with("logical:");
    let tag = if unresolved { " (unresolved)" } else { "" };
    format!(
        "{} {} → {}{tag}",
        e.rel_type, e.source_entity_id, e.target_entity_id
    )
}

/// Knowledge generator: FTS restricted to knowledge/documentation paths.
pub struct KnowledgeGenerator;

impl KnowledgeGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        terms: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_scoped(env, terms, None)
    }

    /// Generate knowledge candidates within an optional path scope.
    pub fn run_scoped(
        env: &mut GeneratorEnv<'_>,
        terms: &[String],
        scope: Option<&attic_storage::PathScope>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let lexical = LexicalGenerator::run_scoped(env, terms, scope)?;
        Ok(lexical
            .into_iter()
            .filter(|c| {
                matches!(
                    c.evidence.source_type,
                    EvidenceSourceType::Knowledge | EvidenceSourceType::Documentation
                )
            })
            .map(|mut c| {
                c.kind = RetrieverKind::Knowledge;
                c.evidence.authority = authority_for(c.evidence.source_type);
                c.evidence.signals.knowledge_authority = Some(match c.evidence.source_type {
                    EvidenceSourceType::Knowledge => 1.0,
                    _ => 0.7,
                });
                c
            })
            .collect())
    }
}

/// Most central-knowledge sections one `context` answer may carry.
pub const CENTRAL_KNOWLEDGE_LIMIT: usize = 8;

/// FTS rows read from the central knowledge repository per query. Wider than
/// [`CENTRAL_KNOWLEDGE_LIMIT`] so the folder's own README chunks, which are
/// filtered out, cannot use up the slots.
const CENTRAL_KNOWLEDGE_READ: usize = 16;

/// True for the central knowledge folder's own `README.md` (instructions
/// for people, not project knowledge). Only the root-level file matches.
pub fn is_central_knowledge_readme(path: &str) -> bool {
    path.eq_ignore_ascii_case("readme.md")
}

/// Central knowledge generator (`[knowledge] dir`): searches ONLY the
/// central knowledge repository, so code elsewhere can never crowd its
/// notes out, and treats every file there as project knowledge.
pub struct CentralKnowledgeGenerator;

impl CentralKnowledgeGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        terms: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        env.limit = env.limit.min(CENTRAL_KNOWLEDGE_READ);
        let lexical = LexicalGenerator::run(env, terms)?;
        Ok(lexical
            .into_iter()
            .filter(|c| !is_central_knowledge_readme(&c.evidence.path))
            .take(CENTRAL_KNOWLEDGE_LIMIT)
            .map(|mut c| {
                c.kind = RetrieverKind::Knowledge;
                c.evidence.source_type = EvidenceSourceType::Knowledge;
                c.evidence.authority = AuthorityLevel::ProjectKnowledge;
                c.evidence.signals.knowledge_authority = Some(1.0);
                c
            })
            .collect())
    }
}

/// Cross-repository dependency generator: produces evidence from Phase 6
/// cross-repository edge and catalog services.
///
/// Uses `attic_storage::crossrepo_ops::cross_edges_touching` to fetch
/// cross-repo DEPENDS_ON edges (the Phase 6 service layer), then filters
/// to edges touching the seed entity IDs. Each edge carries full Phase 6
/// provenance: SourceRevision, resolution level, confidence, and freshness.
pub struct CrossRepoGenerator;

impl CrossRepoGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        seed_entity_ids: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        Self::run_scoped(env, seed_entity_ids, None)
    }

    pub fn run_scoped(
        env: &mut GeneratorEnv<'_>,
        _seed_entity_ids: &[String],
        allowed_repositories: Option<&[String]>,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let mut out = Vec::new();

        // Fetch ALL cross-repo DEPENDS_ON edges unconditionally (bounded at
        // 64). Filtering by seed entity_ids is incorrect here: FTS seeds
        // from a provider repo's go.mod use different IDs than the cross-repo
        // edge source_entity_ids (dependent repo's go.mod). Using cross_edges_all
        // ensures every workspace-level cross-repo edge is evaluated; the
        // BudgetAccountant provides the real candidate ceiling.
        let cross_edges =
            attic_storage::cross_edges_all_scoped(env.conn, 64, allowed_repositories)?;
        for e in cross_edges {
            if !env.budget.admit_candidate() {
                break;
            }
            {
                let resolution = ResolutionLevel::from_db_str(&e.resolution)
                    .unwrap_or(ResolutionLevel::Syntactic);
                // WorkspaceSnapshot provenance is carried by the edge's
                // provenance_json (written by sync_workspace). Parsed, never
                // fabricated; edges without the key stay None.
                let workspace_snapshot_id: Option<String> = e
                    .provenance_json
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok())
                    .and_then(|p| {
                        p.get("workspace_snapshot_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    });
                let mut ev = Evidence::new(
                    uuid::Uuid::new_v4().to_string(),
                    e.source_repository_id.clone(),
                );
                ev.source_type = EvidenceSourceType::Relationship;
                ev.source_id = e.id.clone();
                ev.path = format!(
                    "CROSS_REPO {} [{}] → {} [{}]",
                    e.source_entity_id,
                    e.source_repository_id,
                    e.target_entity_id,
                    e.target_repository_id
                );
                ev.source_revision_id = Some(e.source_revision_id.clone());
                ev.workspace_snapshot_id = workspace_snapshot_id;
                ev.freshness_state = freshness_of(&e.freshness_state);
                ev.authority = AuthorityLevel::Derived;
                ev.confidence = e.confidence.clamp(0.0, 0.99);
                ev.relationship_confidence = Some(e.confidence.clamp(0.0, 0.99));
                ev.relationship = Some(RelationshipProvenance {
                    edge_id: e.id.clone(),
                    rel_type: "DEPENDS_ON".to_owned(),
                    resolution,
                    confidence: e.confidence,
                    hop_depth: 0,
                });
                ev.signals.relationship_confidence = Some(e.confidence);
                ev.signals.structural_proximity = Some(0.5);
                // Relationship evidence carries no source-file snippet; the
                // edge description itself is the renderable content. Without
                // this, context::build's empty-snippet check silently drops
                // every relationship item before it reaches served evidence.
                ev.snippet = Some(ev.path.clone());
                out.push(Candidate::new(RetrieverKind::CrossRepo, ev));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod source_type_for_path_tests {
    use super::*;

    #[test]
    fn legal_notices_are_documentation_not_implementation() {
        for path in [
            "LICENSE",
            "vendor/NOTICE",
            "COPYING",
            "Licence",
            "Copyright",
        ] {
            assert_eq!(
                source_type_for_path(path),
                EvidenceSourceType::Documentation
            );
            assert_eq!(search_label_for_path(path), "documentation");
        }
        assert_eq!(
            source_type_for_path("src/license.rs"),
            EvidenceSourceType::SourceCode
        );
        assert_eq!(
            source_type_for_path("knowledge/LICENSE"),
            EvidenceSourceType::Knowledge
        );
    }

    #[test]
    fn lexical_excerpt_follows_matches_beyond_the_old_prefix() {
        let prefix = "// unrelated boilerplate\n".repeat(100);
        let body = format!("{prefix}fn submit_payment() {{ dispatch_payment(); }}\n// tail\n");
        let (snippet, offset, end) = focused_snippet(
            &body,
            &["submit_payment".into(), "dispatch_payment".into()],
            1200,
        );
        assert!(snippet.contains("fn submit_payment()"));
        assert!(snippet.contains("dispatch_payment"));
        assert!(offset > 0);
        let actual = &body[offset..end];
        assert!(actual.contains("fn submit_payment()"));
        assert!(snippet.len() <= 1200 + '…'.len_utf8());
    }

    #[test]
    fn lexical_term_budget_keeps_late_identifiers() {
        let mut terms: Vec<String> = (0..30).map(|_| "generic".to_string()).collect();
        terms.push("critical_handler_949651".into());
        let selected = prioritize_lexical_terms(&terms);
        assert_eq!(selected.len(), 24);
        assert!(selected.contains(&"critical_handler_949651"));
    }

    #[test]
    fn lexical_excerpt_preserves_unicode_and_no_match_prefix() {
        let body = format!("{}matching_identifier\n", "\u{4e16}\u{754c}\n".repeat(400));
        let (snippet, offset, _) = focused_snippet(&body, &["matching_identifier".into()], 1200);
        assert!(snippet.contains("matching_identifier"));
        assert!(offset > 0);
        let short = "fn unchanged() {}\n";
        assert_eq!(focused_snippet(short, &["missing".into()], 1200).0, short);
    }

    #[test]
    fn lexical_excerpt_does_not_cut_match_to_retain_a_long_leading_line() {
        let body = format!("{}specific_identifier();", " ".repeat(1199));
        let (snippet, _, _) = focused_snippet(&body, &["specific_identifier".into()], 1200);
        assert!(snippet.contains("specific_identifier"));
    }

    #[test]
    fn focused_excerpt_unicode_offsets_and_raw_long_line_are_exact() {
        for token in ["Ölpreis", "İstanbul"] {
            let body = format!("{}{}{}", "x".repeat(2400), token, "y".repeat(2400));
            let (snippet, start, end) = focused_snippet(&body, &[token.to_lowercase()], 1200);
            assert!(start > 0);
            assert!(end < body.len());
            assert!(snippet.contains(token));
            assert_eq!(snippet, body[start..end]);
            assert!(!snippet.contains('…'));
        }
    }

    // Regression coverage for the knowledge/** vs. ordinary-documentation
    // authority boundary documented in README.md / docs/ARCHITECTURE.md —
    // this boundary was previously asserted only in prose, not in a test.

    #[test]
    fn knowledge_prefix_classifies_as_knowledge() {
        assert_eq!(
            source_type_for_path("knowledge/architecture.md"),
            EvidenceSourceType::Knowledge
        );
        assert_eq!(
            source_type_for_path("knowledge/nested/dir/domain.md"),
            EvidenceSourceType::Knowledge
        );
    }

    #[test]
    fn knowledge_prefix_match_is_case_insensitive() {
        assert_eq!(
            source_type_for_path("Knowledge/Domain.MD"),
            EvidenceSourceType::Knowledge
        );
    }

    #[test]
    fn ordinary_docs_outside_knowledge_are_not_elevated() {
        assert_eq!(
            source_type_for_path("README.md"),
            EvidenceSourceType::Documentation
        );
        assert_eq!(
            source_type_for_path("docs/ARCHITECTURE.md"),
            EvidenceSourceType::Documentation
        );
    }

    #[test]
    fn knowledge_in_filename_without_the_path_prefix_is_not_elevated() {
        // A file merely named "knowledge" (or containing the word) outside
        // the `knowledge/` directory must not be silently promoted to
        // ProjectKnowledge authority just because of its filename.
        assert_eq!(
            source_type_for_path("src/knowledge_base.md"),
            EvidenceSourceType::Documentation
        );
    }
}

#[cfg(test)]
mod crossrepo_snapshot_tests {
    use super::*;
    use crate::mode::{AnswerMode, AnswerModePolicy};
    use attic_storage::run_migrations;
    use rusqlite::Connection;

    fn uuid5(name: &str) -> String {
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes()).to_string()
    }

    #[test]
    fn cross_repo_generator_preserves_workspace_snapshot_provenance() {
        let conn = Connection::open_in_memory().unwrap();
        run_migrations(&conn).unwrap();

        // Seed repositories and revisions (core_relationships carries FKs).
        for name in ["repo-dependent", "repo-provider", "repo-other"] {
            let rid = uuid5(name).parse::<attic_core::RepositoryId>().unwrap();
            attic_storage::repository::repository::upsert_repository(
                &conn,
                &rid,
                &format!("C:/tmp/{}", name),
                name,
            )
            .unwrap();
        }
        let mut rev_ids = Vec::new();
        for name in ["repo-dependent", "repo-provider", "repo-other"] {
            let rid = uuid5(name).parse::<attic_core::RepositoryId>().unwrap();
            let srid = attic_core::SourceRevisionId::new_v4();
            attic_storage::repository::source_revision::insert_source_revision(
                &conn,
                &srid,
                &rid,
                "test-sha",
                "2024-01-01",
                attic_core::SourceType::Git,
            )
            .unwrap();
            rev_ids.push(srid.to_string_repr().to_string());
        }

        // Edge WITH snapshot provenance (as written by sync_workspace).
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &uuid5("repo-dependent"),
            "file-occ-dependent",
            &uuid5("repo-provider"),
            "logical:example.com/provider",
            "BUILD_RESOLVED",
            0.9,
            "MANIFEST",
            r#"{"ecosystem":"go","workspace_snapshot_id":"11111111-2222-4333-8444-555555555555"}"#,
            &rev_ids[0],
        )
        .unwrap();
        // Edge WITHOUT a snapshot key: must stay None — never fabricated.
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &uuid5("repo-dependent"),
            "file-occ-dependent",
            &uuid5("repo-other"),
            "logical:example.com/other",
            "SYNTACTIC",
            0.7,
            "MANIFEST",
            "{}",
            &rev_ids[0],
        )
        .unwrap();

        let policy = AnswerModePolicy::for_mode(AnswerMode::Normal);
        let mut budget = BudgetAccountant::new(&policy);
        let mut env = GeneratorEnv {
            conn: &conn,
            repository_id: None,
            budget: &mut budget,
            limit: 16,
        };
        let candidates = CrossRepoGenerator::run(&mut env, &["file-occ-dependent".to_owned()])
            .expect("generator");
        assert!(candidates.len() >= 2, "edges emitted");

        let evs: Vec<&Evidence> = candidates.iter().map(|c| &c.evidence).collect();
        let with_snap = evs
            .iter()
            .find(|e| e.workspace_snapshot_id.is_some())
            .copied()
            .expect("snapshot-backed evidence present");
        assert_eq!(
            with_snap.workspace_snapshot_id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(
            with_snap.source_revision_id.as_deref(),
            Some(rev_ids[0].as_str())
        );
        assert!(
            with_snap.path.contains("example.com/provider"),
            "snapshot evidence is the provider edge: {}",
            with_snap.path
        );

        let without = evs
            .iter()
            .find(|e| e.workspace_snapshot_id.is_none())
            .copied()
            .expect("legacy edge keeps None");
        assert!(without.workspace_snapshot_id.is_none());

        let allowed = vec![uuid5("repo-dependent"), uuid5("repo-provider")];
        let scoped = CrossRepoGenerator::run_scoped(&mut env, &[], Some(&allowed)).unwrap();
        assert_eq!(scoped.len(), 1);
        assert!(scoped[0].evidence.path.contains("example.com/provider"));
        assert!(!scoped[0].evidence.path.contains("example.com/other"));
    }
}
