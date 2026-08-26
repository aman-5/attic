//! Phase 3 — indexing-side structural pipeline.
//!
//! Captures canonical structural payloads produced by specialized analyzers,
//! upgrades import/heritage edges against repository layout and symbol
//! tables (never beyond actual evidence), and emits publication-ready
//! [`PublicationStructuralFile`] entries.
//!
//! # Resolution honesty
//!
//! An edge is upgraded ONLY when its target is confirmed:
//! - Java import `p.q.C` → candidate source file exists AND (optionally) the
//!   class is a known symbol → `SYMBOL_RESOLVED` / `PACKAGE_RESOLVED`.
//! - Go import under the `go.mod` module prefix → package dir exists in the
//!   manifest → `PACKAGE_RESOLVED`, basis `GO_MODULE`.
//! - Python relative/dotted imports mapped onto repo layout →
//!   `PACKAGE_RESOLVED`.
//! - JS/TS relative specifiers probed against the manifest →
//!   `PACKAGE_RESOLVED` (basis stays `IMPORT`; npm registry knowledge is NOT
//!   claimed).
//! - Heritage (`EXTENDS`/`IMPLEMENTS`) whose type resolves to a known symbol
//!   definition (same run or DB) → `SYMBOL_RESOLVED`.
//!
//! Everything else remains `SYNTACTIC` with an honest confidence ≤ 0.6.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use attic_analyzers::{Analyzer, GenericAnalyzer, ImportSpec, ResolutionLevel};
use serde_json::json;

use attic_storage::{
    PublicationNode, PublicationRelationship, PublicationStructuralFile, PublicationSymbolDef,
};

/// Registry with GenericAnalyzer plus every bundled structural language.
pub(crate) fn default_registry() -> attic_analyzers::AnalyzerRegistry {
    attic_analyzers::default_registry()
}

/// Registry with ONLY the GenericAnalyzer — the Phase 1D baseline used by
/// `IndexOptions { structural: false }` (benchmarks / kill-switch).
pub(crate) fn generic_only_registry() -> attic_analyzers::AnalyzerRegistry {
    attic_analyzers::AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>)
}

// ---------------------------------------------------------------------------
// Captured per-file analysis
// ---------------------------------------------------------------------------

/// A relationship edge exactly as the analyzer emitted it (pre-resolution).
#[derive(Debug, Clone)]
struct RawRel {
    rel_type: String,
    target: String,
    resolution: ResolutionLevel,
    confidence: f64,
    source_symbol_index: Option<usize>,
}

/// One analyzed file awaiting resolution + publication conversion.
#[derive(Debug)]
pub(crate) struct CapturedFile {
    file_occurrence_id: String,
    rel_path: String,
    analyzer_id: String,
    analyzer_version: String,
    language_tag: String,
    /// `false` when the analyzer reported PARTIAL structural coverage
    /// (prefix truncation, entity caps, mid-extraction stop). Persisted so
    /// partial structure is never presented as complete.
    structurally_complete: bool,
    nodes: Vec<PublicationNode>,
    symbols: Vec<PublicationSymbolDef>,
    raw_rels: Vec<RawRel>,
    imports: Vec<ImportSpec>,
}

/// Capture the structural part of an `AnalyzerOutput`.
///
/// Returns `None` when the output carries no structural intelligence or came
/// from the generic fallback (nothing to persist).
pub(crate) fn capture_structural(
    rel_path: &str,
    file_occurrence_id: &str,
    output: &attic_analyzers::AnalyzerOutput,
) -> Option<CapturedFile> {
    if output.fallback_used {
        return None;
    }
    if output.structural_nodes.is_empty() && output.symbols.is_empty() && output.imports.is_empty()
    {
        return None;
    }
    let language_tag = output
        .analyzer_id
        .split('-')
        .next()
        .unwrap_or("")
        .to_string();
    if language_tag.is_empty() {
        return None;
    }

    let nodes = output
        .structural_nodes
        .iter()
        .map(|n| PublicationNode {
            parent_index: n.parent_index,
            node_type: n.node_type.clone(),
            structural_identity: n.structural_identity.clone(),
            span_str: n.span.to_string(),
            content_hash: n.content_hash.clone(),
            metadata_json: n.metadata_json.clone(),
        })
        .collect();

    let symbols = output
        .symbols
        .iter()
        .map(|s| PublicationSymbolDef {
            language: language_tag.clone(),
            qualified_name: s.qualified_name.clone(),
            kind: s.kind.as_str().to_string(),
            disambiguator: s.disambiguator.clone(),
            span_str: s.definition_span.to_string(),
            signature: s.signature.clone(),
            visibility: s.visibility.clone(),
            is_definition: s.is_definition,
        })
        .collect();

    let raw_rels = output
        .relationships
        .iter()
        .map(|r| RawRel {
            rel_type: r.relationship_type.clone(),
            target: r.target_qualified_name.clone(),
            resolution: r.resolution,
            confidence: r.confidence,
            source_symbol_index: r.source_symbol_index,
        })
        .collect();

    Some(CapturedFile {
        file_occurrence_id: file_occurrence_id.to_string(),
        rel_path: rel_path.to_string(),
        analyzer_id: output.analyzer_id.clone(),
        analyzer_version: output.analyzer_version.clone(),
        language_tag,
        structurally_complete: output.structurally_complete,
        nodes,
        symbols,
        raw_rels,
        imports: output.imports.clone(),
    })
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// External lookups supplied by the caller (DB access stays outside this
/// module so it can be unit-tested purely).
pub(crate) struct ResolverDeps<'a> {
    /// `(qualified_name, [kinds]) -> defining file occurrence UUID`
    pub symbol_definition: &'a dyn Fn(&str, &[&str]) -> Option<String>,
    /// `repo_relative path -> latest file occurrence UUID`
    pub path_occurrence: &'a dyn Fn(&str) -> Option<String>,
}

pub(crate) struct StructuralPipeline {
    #[allow(dead_code)]
    repo_root: PathBuf,
    known_paths: BTreeSet<String>,
    go_module_prefix: Option<String>,
    files: Vec<CapturedFile>,
    /// qualified name → defining repo-relative path (this run only).
    in_run_symbols: HashMap<String, String>,
    /// repo-relative path → occurrence UUID for files published THIS run.
    path_to_occ_in_run: HashMap<String, String>,
}

impl StructuralPipeline {
    pub(crate) fn new(repo_root: &Path, known_paths: BTreeSet<String>) -> Self {
        let go_module_prefix = read_go_module_prefix(repo_root);
        Self {
            repo_root: repo_root.to_path_buf(),
            known_paths,
            go_module_prefix,
            files: Vec::new(),
            in_run_symbols: HashMap::new(),
            path_to_occ_in_run: HashMap::new(),
        }
    }

    /// Register a path→occurrence mapping for ANY file published this run
    /// (not only those with structural payloads) so import edges can target
    /// them without depending on not-yet-committed DB rows.
    pub(crate) fn note_occurrence(&mut self, rel_path: &str, occurrence_id: &str) {
        self.path_to_occ_in_run
            .insert(rel_path.to_string(), occurrence_id.to_string());
    }

    pub(crate) fn record(&mut self, captured: CapturedFile) {
        for s in &captured.symbols {
            if s.is_definition {
                self.in_run_symbols
                    .entry(s.qualified_name.clone())
                    .or_insert_with(|| captured.rel_path.clone());
                // Short-name alias for heritage matching (first wins, stable
                // by processing order which itself is manifest-sorted).
                let short = s.qualified_name.rsplit('.').next().unwrap_or("");
                self.in_run_symbols
                    .entry(short.to_string())
                    .or_insert_with(|| captured.rel_path.clone());
            }
        }
        self.path_to_occ_in_run.insert(
            captured.rel_path.clone(),
            captured.file_occurrence_id.clone(),
        );
        self.files.push(captured);
    }

    /// Run the resolution pass and produce publication payloads.
    pub(crate) fn finish(
        mut self,
        deps: &ResolverDeps<'_>,
        unit_links_by_occ: &HashMap<String, Vec<(String, usize)>>,
    ) -> Vec<PublicationStructuralFile> {
        let mut out = Vec::with_capacity(self.files.len());
        // Deterministic order: by repo-relative path.
        self.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
        let files = std::mem::take(&mut self.files);
        // Symbol-evidence checker for resolvers: in-run table first, then DB.
        let symbols_known = |qname: &str| -> bool {
            if self.in_run_symbols.contains_key(qname) {
                return true;
            }
            (deps.symbol_definition)(qname, &["class", "interface"]).is_some()
        };
        let known_paths = self.known_paths.clone();
        let go_prefix = self.go_module_prefix.clone();
        for f in files {
            let mut relationships: Vec<PublicationRelationship> = Vec::new();

            // ── Imports ──────────────────────────────────────────────────────
            for imp in &f.imports {
                let upgrade = resolve_import(
                    &f.language_tag,
                    &f.rel_path,
                    &imp.raw_specifier,
                    imp.import_kind.as_str(),
                    &known_paths,
                    &go_prefix,
                    &&symbols_known,
                );
                let (resolved, target_id, resolution, basis, confidence): (
                    bool,
                    String,
                    ResolutionLevel,
                    &'static str,
                    f64,
                ) = match upgrade {
                    Some(u) => match u.target {
                        // Path matched; resolve to an occurrence when possible.
                        ResolvedTarget::Path(p) => {
                            match self.path_to_occ_in_run.get(&p).cloned() {
                                Some(occ) => (true, occ, u.resolution, u.basis, u.confidence),
                                // Path known to the manifest but not published
                                // this run → DB lookup; else stay syntactic.
                                None => match (deps.path_occurrence)(&p) {
                                    Some(occ) => (true, occ, u.resolution, u.basis, u.confidence),
                                    None => (
                                        false,
                                        imp.raw_specifier.clone(),
                                        ResolutionLevel::Syntactic,
                                        "IMPORT",
                                        0.5,
                                    ),
                                },
                            }
                        }
                    },
                    None => (
                        false,
                        imp.raw_specifier.clone(),
                        ResolutionLevel::Syntactic,
                        "IMPORT",
                        0.5,
                    ),
                };
                relationships.push(PublicationRelationship {
                    rel_type: "IMPORT".to_string(),
                    target_entity_id: target_id,
                    resolved,
                    dependency_basis: basis.to_string(),
                    resolution: resolution.as_db_str().to_string(),
                    confidence,
                    source_symbol_index: None,
                    provenance_json: Some(
                        json!({
                            "kind": imp.import_kind,
                            "specifier": imp.raw_specifier,
                            "file": f.rel_path,
                            "span": imp.span.to_string(),
                        })
                        .to_string(),
                    ),
                });
            }

            // ── Heritage / other symbol-level edges ─────────────────────────
            for rel in &f.raw_rels {
                let (resolved_target, resolved, resolution, confidence) =
                    match rel.rel_type.as_str() {
                        "EXTENDS" | "IMPLEMENTS" => {
                            let kinds: &[&str] = if rel.rel_type == "EXTENDS" {
                                &["class", "interface"]
                            } else {
                                &["interface", "class"]
                            };
                            match self.lookup_type(&rel.target, kinds, deps) {
                                Some(occ) => (occ, true, ResolutionLevel::SymbolResolved, 0.9_f64),
                                None => (rel.target.clone(), false, rel.resolution, rel.confidence),
                            }
                        }
                        _ => (rel.target.clone(), false, rel.resolution, rel.confidence),
                    };
                relationships.push(PublicationRelationship {
                    rel_type: rel.rel_type.clone(),
                    target_entity_id: resolved_target,
                    resolved,
                    dependency_basis: "IMPORT".to_string(),
                    resolution: resolution.as_db_str().to_string(),
                    confidence,
                    source_symbol_index: rel.source_symbol_index,
                    provenance_json: Some(
                        json!({
                            "target_name": rel.target,
                            "file": f.rel_path,
                        })
                        .to_string(),
                    ),
                });
            }

            let occ = f.file_occurrence_id.clone();
            let unit_links = unit_links_by_occ
                .get(&occ)
                .map(|links| {
                    links
                        .iter()
                        .enumerate()
                        .map(|(ordinal, (uid, idx))| attic_storage::PublicationUnitLink {
                            retrieval_unit_id: uid.clone(),
                            node_index: *idx,
                            ordinal: ordinal as u32,
                        })
                        .collect()
                })
                .unwrap_or_default();

            out.push(PublicationStructuralFile {
                file_occurrence_id: occ,
                structurally_complete: f.structurally_complete,
                analyzer_id: f.analyzer_id,
                analyzer_version: f.analyzer_version,
                nodes: f.nodes,
                symbols: f.symbols,
                relationships,
                unit_links,
            });
        }
        out
    }

    /// Look up a TYPE by simple or qualified name; same run first, then DB.
    fn lookup_type(&self, name: &str, kinds: &[&str], deps: &ResolverDeps<'_>) -> Option<String> {
        let mut candidate_paths: Vec<String> = Vec::new();
        if let Some(p) = self.in_run_symbols.get(name) {
            candidate_paths.push(p.clone());
        }
        let suffix_key = format!(".{name}");
        let mut suffix_hits: Vec<String> = self
            .in_run_symbols
            .iter()
            .filter(|(k, _)| k.ends_with(&suffix_key))
            .map(|(_, v)| v.clone())
            .collect();
        suffix_hits.sort();
        candidate_paths.extend(suffix_hits);

        for p in candidate_paths {
            if let Some(occ) = self
                .path_to_occ_in_run
                .get(&p)
                .cloned()
                .or_else(|| (deps.path_occurrence)(&p))
            {
                return Some(occ);
            }
        }
        // DB fallback by qualified or short name.
        (deps.symbol_definition)(name, kinds)
    }
}

// ---------------------------------------------------------------------------
// Import resolution per language (adapter point for future languages)
// ---------------------------------------------------------------------------

struct Upgrade {
    target: ResolvedTarget,
    resolution: ResolutionLevel,
    basis: &'static str,
    confidence: f64,
}

enum ResolvedTarget {
    /// A repo-relative path confirmed present in the manifest.
    Path(String),
}

#[allow(clippy::too_many_arguments)]
fn resolve_import(
    language_tag: &str,
    importer_rel: &str,
    specifier: &str,
    _kind: &str,
    known_paths: &BTreeSet<String>,
    go_module_prefix: &Option<String>,
    symbols_known: &dyn Fn(&str) -> bool,
) -> Option<Upgrade> {
    match language_tag {
        "java" => resolve_java(importer_rel, specifier, known_paths, symbols_known),
        "go" => resolve_go(specifier, known_paths, go_module_prefix),
        "python" => resolve_python(importer_rel, specifier, known_paths),
        "javascript" | "typescript" => resolve_js_ts(importer_rel, specifier, known_paths),
        _ => {
            let _ = importer_rel;
            None
        }
    }
}

fn path_occ(path: &str, known: &BTreeSet<String>) -> bool {
    known.contains(path)
}

fn first_known(
    candidates: impl Iterator<Item = String>,
    known: &BTreeSet<String>,
) -> Option<String> {
    candidates.into_iter().find(|c| path_occ(c, known))
}

fn resolve_java(
    _importer: &str,
    specifier: &str,
    known: &BTreeSet<String>,
    symbols_known: &dyn Fn(&str) -> bool,
) -> Option<Upgrade> {
    let spec = specifier.strip_suffix(".*").unwrap_or(specifier);
    let parts: Vec<&str> = spec.split(':').flat_map(|s| s.split('.')).collect();
    if parts.len() < 2 {
        return None;
    }
    let rel = parts.join("/");
    let prefixes = ["", "src/main/java/", "src/test/java/", "src/"];
    let cands = prefixes
        .iter()
        .flat_map(|p| [format!("{p}{rel}.java"), format!("{p}{rel}.kt")]);
    first_known(cands, known).map(|path| {
        if symbols_known(spec) {
            // The imported FQN is a known class/interface definition.
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::SymbolResolved,
                basis: "IMPORT",
                confidence: 0.95,
            }
        } else {
            // Only the file layout matched.
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::PackageResolved,
                basis: "IMPORT",
                confidence: 0.85,
            }
        }
    })
}

fn resolve_go(
    specifier: &str,
    known: &BTreeSet<String>,
    module_prefix: &Option<String>,
) -> Option<Upgrade> {
    let Some(prefix) = module_prefix else {
        return None;
    };
    let rel = specifier
        .strip_prefix(prefix.as_str())?
        .trim_start_matches('/')
        .to_string();
    if rel.is_empty() {
        return None;
    }
    // Any manifest file under the package directory represents the package.
    let dir_prefix = format!("{rel}/");
    let hit = known
        .range(dir_prefix.clone()..)
        .take_while(|p| p.starts_with(&dir_prefix))
        .next()
        .cloned();
    hit.map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "GO_MODULE",
        confidence: 0.9,
    })
}

fn python_candidates(importer_rel: &str, module_part: &str) -> Vec<String> {
    let dots = module_part.chars().take_while(|c| *c == '.').count();
    let tail = &module_part[dots..];
    let importer_dir = Path::new(importer_rel)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_default();
    // '.' = current package dir; '..' climbs one level.
    let mut base = importer_dir;
    for _ in 1..dots {
        base.pop();
    }
    let tail_path = if tail.is_empty() {
        base.clone()
    } else {
        base.join(tail.replace('.', "/"))
    };
    let tp = tail_path.to_string_lossy().replace('\\', "/");
    vec![format!("{tp}.py"), format!("{tp}/__init__.py")]
}

fn resolve_python(
    importer_rel: &str,
    specifier: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    // Encoded by the analyzer as "<module>:<name>" / "<module>:*".
    let (module_part, _name) = specifier.split_once(':').unwrap_or((specifier, ""));
    let cands = python_candidates(importer_rel, module_part);
    first_known(cands.into_iter(), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "PYTHON_PACKAGE",
        confidence: 0.85,
    })
}

const JS_PROBE_SUFFIXES: [&str; 12] = [
    "",
    ".ts",
    ".tsx",
    ".js",
    ".jsx",
    ".mjs",
    ".cjs",
    "/index.ts",
    "/index.tsx",
    "/index.js",
    "/index.jsx",
    "/index.mjs",
];

fn resolve_js_ts(importer_rel: &str, specifier: &str, known: &BTreeSet<String>) -> Option<Upgrade> {
    if !specifier.starts_with("./") && !specifier.starts_with("../") && !specifier.starts_with('/')
    {
        return None; // bare npm-style specifier stays syntactic
    }
    let base_dir = if specifier.starts_with('/') {
        String::new()
    } else {
        Path::new(importer_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default()
    };
    let joined = normalize_rel(&format!("{base_dir}/{specifier}"));
    let cands = JS_PROBE_SUFFIXES
        .iter()
        .map(move |s| format!("{joined}{s}"));
    first_known(cands, known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "IMPORT",
        confidence: 0.8,
    })
}

fn normalize_rel(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// Read `module <path>` from `<root>/go.mod` (bounded read, silent degrade).
fn read_go_module_prefix(root: &Path) -> Option<String> {
    let bytes = std::fs::read(root.join("go.mod")).ok()?;
    if bytes.len() > 64 * 1024 {
        return None;
    }
    let text = String::from_utf8_lossy(&bytes);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("module ") {
            return Some(rest.trim().trim_matches('"').to_string());
        }
    }
    None
}
