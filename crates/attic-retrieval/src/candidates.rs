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
    /// Phase 5: nearest-neighbor over the disposable semantic layer.
    Semantic,
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
        "SEMANTIC" | "VECTOR" => Some(RetrieverKind::Semantic),
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
            Self::Semantic => "SEMANTIC",
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
pub fn source_type_for_path(path: &str) -> EvidenceSourceType {
    let lower = path.to_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    if lower.starts_with("knowledge/") || name == "readme.md" || name == "architecture.md" {
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
    if DOC_EXTS.iter().any(|e| name.ends_with(e)) {
        return EvidenceSourceType::Documentation;
    }
    if name.ends_with(".json") {
        return EvidenceSourceType::Configuration;
    }
    EvidenceSourceType::SourceCode
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
    Some(attic_core::SourceSpan::new(
        start_line.unwrap_or(0),
        0,
        end_line.unwrap_or(start_line?.saturating_add(1)),
        0,
    ))
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
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        // Deterministic phrase construction: `"term1" OR "term2"`.
        let fts_query = terms
            .iter()
            .take(6)
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
        let hits = attic_storage::fts_search(env.conn, &params)?;
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
            ev.snippet = Some(bound_snippet(&h.body, 1200));
            ev.signals.lexical_score = Some(attic_evidence::signals::normalize_lexical(h.score));
            ev.source_span = unit_span(h.start_line, h.end_line);
            fill_file_provenance(env.conn, &mut ev);
            let mut c = Candidate::new(RetrieverKind::Fts, ev);
            c.evidence.retrieval_sources[0].query_fragment = fts_query.clone();
            out.push(c);
        }
        Ok(out)
    }
}

/// Exact path lookup generator (uses `fts_path_lookup` + header provenance).
pub struct PathExactGenerator;

impl PathExactGenerator {
    pub fn run(env: &mut GeneratorEnv<'_>, path: &str) -> Result<Vec<Candidate>, RetrievalError> {
        let hits =
            attic_storage::fts_path_lookup(env.conn, path, repo_filter(env), env.limit.min(64))?;
        let mut out = Vec::new();
        for h in hits {
            if !env.budget.admit_candidate() {
                break;
            }
            let st = source_type_for_path(&h.path);
            let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), h.repository_id.clone());
            ev.source_type = st;
            ev.source_id = h.file_occurrence_id;
            ev.path = h.path;
            ev.freshness_state = freshness_of(&h.freshness_state);
            ev.authority = authority_for(ev.source_type);
            ev.confidence = 1.0; // exact path match
            ev.snippet = Some(bound_snippet(&h.body, 1200));
            ev.signals.symbol_match_score = Some(1.0);
            ev.source_span = unit_span(h.start_line, h.end_line);
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
        if name.trim().is_empty() {
            return Ok(Vec::new());
        }
        let exact = attic_storage::lookup_symbol_exact(env.conn, repo_filter(env), name, 16)?;
        let exact_match = !exact.is_empty();
        let fuzzy = if exact.is_empty() {
            attic_storage::search_symbols(env.conn, repo_filter(env), name, env.limit.min(64))?
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

    /// Nodes by node-type fragment within one repository (architecture
    /// questions with no symbol hint).
    pub fn run_by_type(
        env: &mut GeneratorEnv<'_>,
        node_type_like: &str,
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let Some(repo) = repo_filter(env).map(str::to_owned) else {
            return Ok(Vec::new());
        };
        let rows = attic_storage::structural_nodes_by_type(
            env.conn,
            &repo,
            node_type_like,
            env.limit.min(48),
        )?;
        let mut out = Vec::new();
        for n in rows.into_iter().filter(|_| env.budget.admit_candidate()) {
            let header = attic_storage::file_header_by_id(env.conn, &n.file_occurrence_id)?;
            let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), n.id.clone());
            if let Some(h) = &header {
                ev.repository_id = h.repository_id.clone();
                ev.source_revision_id = Some(h.source_revision_id.clone());
                ev.index_generation_id = h.index_generation_id.clone();
                ev.content_hash = Some(h.content_hash.clone());
            }
            ev.source_type = EvidenceSourceType::SourceCode;
            ev.source_id = n.file_occurrence_id.clone();
            ev.path = n.path.clone();
            ev.freshness_state = freshness_of(&n.freshness_state);
            ev.authority = AuthorityLevel::Implementation;
            ev.confidence = 0.45;
            ev.signals.structural_proximity = Some(0.8);
            out.push(Candidate::new(RetrieverKind::Structural, ev));
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
        let mut out = Vec::new();
        for entity in seed_entity_ids.iter().take(16) {
            if !env.budget.candidates_available() {
                break;
            }
            for e in attic_storage::relationships_for_entity(env.conn, entity, 24)? {
                if !env.budget.admit_candidate() {
                    break;
                }
                let resolution = ResolutionLevel::from_db_str(&e.resolution)
                    .unwrap_or(ResolutionLevel::Syntactic);
                let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), String::new());
                ev.source_type = EvidenceSourceType::Relationship;
                ev.source_id = e.id.clone();
                ev.path = relationship_label(&e);
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
        let lexical = LexicalGenerator::run(env, terms)?;
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

/// Cross-repository dependency generator: produces evidence from cross-repo
/// DEPENDS_ON edges where source and target reside in different repositories.
pub struct CrossRepoGenerator;

impl CrossRepoGenerator {
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        seed_entity_ids: &[String],
    ) -> Result<Vec<Candidate>, RetrievalError> {
        let mut out = Vec::new();
        for entity in seed_entity_ids.iter().take(16) {
            if !env.budget.candidates_available() {
                break;
            }
            for e in attic_storage::relationships_for_entity(env.conn, entity, 24)? {
                // Only emit cross-repo edges (source != target repository).
                if e.source_repository_id == e.target_repository_id {
                    continue;
                }
                if e.rel_type != "DEPENDS_ON" {
                    continue;
                }
                if !env.budget.admit_candidate() {
                    break;
                }
                let resolution = ResolutionLevel::from_db_str(&e.resolution)
                    .unwrap_or(ResolutionLevel::Syntactic);
                let mut ev = Evidence::new(uuid::Uuid::new_v4().to_string(), e.source_repository_id.clone());
                ev.source_type = EvidenceSourceType::Relationship;
                ev.source_id = e.id.clone();
                ev.path = format!(
                    "CROSS_REPO {} [{}] → {} [{}]",
                    e.source_entity_id, e.source_repository_id,
                    e.target_entity_id, e.target_repository_id
                );
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
                ev.signals.structural_proximity = Some(0.5);
                out.push(Candidate::new(RetrieverKind::CrossRepo, ev));
            }
        }
        Ok(out)
    }
}
