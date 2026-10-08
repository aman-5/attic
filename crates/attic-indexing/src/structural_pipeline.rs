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
use std::path::Path;
use std::sync::Arc;

use attic_analyzers::{Analyzer, GenericAnalyzer, ImportSpec, ResolutionLevel};
use serde_json::json;

use attic_storage::{
    PublicationNode, PublicationRelationship, PublicationStructuralFile, PublicationSymbolDef,
};

/// Registry with ONLY the GenericAnalyzer — the Phase 1D baseline used by
/// `IndexOptions { structural: false }` (benchmarks / kill-switch).
pub(crate) fn generic_only_registry() -> attic_analyzers::AnalyzerRegistry {
    attic_analyzers::AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>)
}

/// The registry an indexing run should use, built once per distinct
/// analyzer configuration and shared by every run (full and incremental)
/// in the process. Analyzers are `Send + Sync` and read-only during
/// `analyze`, so one instance safely serves concurrent runs.
///
/// Registries are built outside the cache lock (building compiles tags
/// queries), so a first build never blocks runs using another configuration.
/// A poisoned lock is recovered rather than propagated: the map only ever
/// holds fully built registries.
pub(crate) fn shared_registry(
    opts: &crate::IndexOptions,
) -> Result<Arc<attic_analyzers::AnalyzerRegistry>, crate::IndexError> {
    type Key = (bool, attic_analyzers::AnalyzerSelection);
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<HashMap<Key, Arc<attic_analyzers::AnalyzerRegistry>>>,
    > = std::sync::OnceLock::new();

    let catalog = attic_analyzers::PluginCatalog::builtin();
    // Fail closed on unknown plugin ids even when structural analysis is
    // off, so a typo never lies dormant until the kill-switch is flipped.
    catalog
        .validate(&opts.analyzers)
        .map_err(|e| crate::IndexError::AnalyzerConfig(e.to_string()))?;
    let key: Key = if opts.structural {
        (true, opts.analyzers.clone())
    } else {
        (false, attic_analyzers::AnalyzerSelection::all())
    };

    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(Arc::clone(found));
    }

    let built = Arc::new(if key.0 {
        catalog
            .build_registry(&key.1)
            .map_err(|e| crate::IndexError::AnalyzerConfig(e.to_string()))?
    } else {
        generic_only_registry()
    });
    let mut guard = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Ok(Arc::clone(guard.entry(key).or_insert(built)))
}

// ---------------------------------------------------------------------------
// Captured per-file analysis
// ---------------------------------------------------------------------------

/// A relationship edge exactly as the analyzer emitted it (pre-resolution).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct RawRel {
    rel_type: String,
    target: String,
    resolution: ResolutionLevel,
    confidence: f64,
    source_symbol_index: Option<usize>,
}

/// One analyzed file awaiting resolution + publication conversion.
///
/// PR-7: also the unit cached by `index_analysis_cache` so a full-index
/// retry can reuse a file's structural capture instead of re-analyzing it.
/// `Serialize`/`Deserialize` support that cache; they are not used for any
/// other persistence path.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
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

impl CapturedFile {
    /// Retarget this capture to the current run's freshly generated
    /// `file_occurrence_id` (PR-7 cache reuse): a cached capture was
    /// serialized against a *previous*, never-published attempt's
    /// occurrence id, which must not leak into this run's publication.
    pub(crate) fn retarget_file_occurrence_id(&mut self, new_id: String) {
        self.file_occurrence_id = new_id;
    }
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
    kind: &str,
    known_paths: &BTreeSet<String>,
    go_module_prefix: &Option<String>,
    symbols_known: &dyn Fn(&str) -> bool,
) -> Option<Upgrade> {
    match language_tag {
        "java" => resolve_java(importer_rel, specifier, known_paths, symbols_known),
        "c" => resolve_c(importer_rel, specifier, kind, known_paths),
        "cpp" => resolve_cpp(importer_rel, specifier, kind, known_paths),
        "csharp" => resolve_csharp(importer_rel, specifier, known_paths),
        "kotlin" => resolve_kotlin(specifier, known_paths, symbols_known),
        "scala" => resolve_scala(specifier, known_paths, symbols_known),
        "go" => resolve_go(specifier, known_paths, go_module_prefix),
        "lua" => resolve_lua(specifier, known_paths),
        "python" => resolve_python(importer_rel, specifier, known_paths),
        "ruby" => resolve_ruby(importer_rel, specifier, kind, known_paths),
        "php" => resolve_php(importer_rel, specifier, kind, known_paths),
        "swift" => resolve_swift(specifier, known_paths),
        "rust" => resolve_rust(importer_rel, specifier, kind, known_paths),
        "dockerfile" => resolve_dockerfile(importer_rel, specifier, kind, known_paths),
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

fn resolve_kotlin(
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
    let prefixes = [
        "",
        "src/main/kotlin/",
        "src/test/kotlin/",
        "src/main/java/",
        "src/test/java/",
        "src/",
    ];
    let cands = prefixes.iter().flat_map(|p| {
        [
            format!("{p}{rel}.kt"),
            format!("{p}{rel}.kts"),
            format!("{p}{rel}.java"),
        ]
    });
    first_known(cands, known).map(|path| {
        if symbols_known(spec) {
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::SymbolResolved,
                basis: "IMPORT",
                confidence: 0.95,
            }
        } else {
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::PackageResolved,
                basis: "IMPORT",
                confidence: 0.85,
            }
        }
    })
}

fn resolve_scala(
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
    let prefixes = [
        "",
        "src/main/scala/",
        "src/test/scala/",
        "src/main/java/",
        "src/test/java/",
        "src/",
    ];
    let cands = prefixes
        .iter()
        .flat_map(|p| [format!("{p}{rel}.scala"), format!("{p}{rel}.java")]);
    first_known(cands, known).map(|path| {
        if symbols_known(spec) {
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::SymbolResolved,
                basis: "IMPORT",
                confidence: 0.95,
            }
        } else {
            Upgrade {
                target: ResolvedTarget::Path(path),
                resolution: ResolutionLevel::PackageResolved,
                basis: "IMPORT",
                confidence: 0.85,
            }
        }
    })
}

fn resolve_lua(specifier: &str, known: &BTreeSet<String>) -> Option<Upgrade> {
    if specifier.is_empty() {
        return None;
    }
    let rel = specifier.replace('.', "/");
    let prefixes = ["", "lua/", "src/", "src/lua/"];
    let cands = prefixes
        .iter()
        .flat_map(|p| [format!("{p}{rel}.lua"), format!("{p}{rel}/init.lua")]);
    first_known(cands, known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "IMPORT",
        confidence: 0.8,
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

const C_LIKE_INCLUDE_ROOTS: [&str; 4] = ["include/", "inc/", "src/", ""];

fn resolve_c(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    resolve_c_like(importer_rel, specifier, kind, known, false)
}

fn resolve_cpp(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    resolve_c_like(importer_rel, specifier, kind, known, true)
}

fn resolve_c_like(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
    cpp: bool,
) -> Option<Upgrade> {
    if specifier.is_empty() {
        return None;
    }

    let variants = include_variants(specifier, cpp);
    let mut candidates: Vec<(String, f64)> = Vec::new();

    if kind == "INCLUDE_QUOTE" {
        let importer_dir = Path::new(importer_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        for variant in &variants {
            candidates.push((join_rel(&importer_dir, variant), 0.9));
        }
    } else if kind != "INCLUDE_ANGLE" {
        return None;
    }

    for root in C_LIKE_INCLUDE_ROOTS {
        for variant in &variants {
            candidates.push((
                join_rel(root, variant),
                if kind == "INCLUDE_QUOTE" { 0.85 } else { 0.8 },
            ));
        }
    }

    for (candidate, confidence) in candidates {
        if path_occ(&candidate, known) {
            return Some(Upgrade {
                target: ResolvedTarget::Path(candidate),
                resolution: ResolutionLevel::PackageResolved,
                basis: "IMPORT",
                confidence,
            });
        }
    }
    None
}

fn include_variants(specifier: &str, cpp: bool) -> Vec<String> {
    let normalized = specifier
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    let mut out = vec![normalized.clone()];
    if cpp && Path::new(&normalized).extension().is_none() {
        for ext in [".hpp", ".hh", ".hxx", ".inl"] {
            out.push(format!("{normalized}{ext}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

fn join_rel(prefix: &str, specifier: &str) -> String {
    let base = prefix.trim_end_matches('/');
    let child = specifier.trim_start_matches('/');
    if base.is_empty() {
        child.to_string()
    } else {
        format!("{base}/{child}")
    }
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

fn resolve_csharp(
    importer_rel: &str,
    specifier: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    let spec = specifier
        .trim()
        .strip_suffix(".*")
        .unwrap_or(specifier.trim());
    if spec.is_empty() {
        return None;
    }
    let rel = spec.replace("::", "/").replace('.', "/");
    let mut cands = vec![format!("{rel}.cs"), format!("src/{rel}.cs")];
    if let Some(project) = csharp_project_prefix(importer_rel, known) {
        cands.push(normalize_rel(&format!("{project}/{rel}.cs")));
        cands.push(normalize_rel(&format!("{project}/src/{rel}.cs")));
    }
    first_known(cands.into_iter().map(|c| normalize_rel(&c)), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "CSHARP_NAMESPACE",
        confidence: 0.85,
    })
}

fn csharp_project_prefix(importer_rel: &str, known: &BTreeSet<String>) -> Option<String> {
    let mut dir = Path::new(importer_rel).parent()?.to_path_buf();
    loop {
        let prefix = dir.to_string_lossy().replace('\\', "/");
        let probe = format!("{prefix}/");
        if !prefix.is_empty()
            && known
                .iter()
                .any(|p| p.starts_with(&probe) && p.ends_with(".csproj"))
        {
            return Some(prefix);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn rust_crate_src_root(importer_rel: &str) -> Option<String> {
    let parts: Vec<&str> = importer_rel.split('/').collect();
    let src_idx = parts.iter().rposition(|seg| *seg == "src")?;
    Some(parts[..=src_idx].join("/"))
}

fn rust_current_module_dir(importer_rel: &str) -> String {
    let parent = Path::new(importer_rel)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let file_name = Path::new(importer_rel)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        parent
    } else {
        let stem = Path::new(importer_rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        normalize_rel(&format!("{parent}/{stem}"))
    }
}

fn rust_module_file_candidates(base: &str, segments: &[&str]) -> Vec<String> {
    if segments.is_empty() {
        return Vec::new();
    }
    let joined = if base.is_empty() {
        segments.join("/")
    } else {
        format!("{base}/{}", segments.join("/"))
    };
    vec![
        normalize_rel(&format!("{joined}.rs")),
        normalize_rel(&format!("{joined}/mod.rs")),
    ]
}

fn rust_use_candidates(importer_rel: &str, specifier: &str) -> Vec<String> {
    let spec = specifier.strip_suffix("::*").unwrap_or(specifier);
    let mut segments: Vec<&str> = spec.split("::").filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Vec::new();
    }

    let base = match segments[0] {
        "crate" => {
            segments.remove(0);
            rust_crate_src_root(importer_rel).unwrap_or_default()
        }
        "self" => {
            segments.remove(0);
            rust_current_module_dir(importer_rel)
        }
        "super" => {
            let mut base = Path::new(&rust_current_module_dir(importer_rel)).to_path_buf();
            while segments.first().copied() == Some("super") {
                segments.remove(0);
                base.pop();
            }
            base.to_string_lossy().replace('\\', "/")
        }
        _ => rust_crate_src_root(importer_rel).unwrap_or_default(),
    };

    let mut out = rust_module_file_candidates(&base, &segments);
    if segments.len() > 1 {
        out.extend(rust_module_file_candidates(
            &base,
            &segments[..segments.len() - 1],
        ));
    }
    out
}

fn resolve_rust(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    if kind == "EXTERN_CRATE" {
        return None;
    }
    let cands = if kind == "MOD" {
        let base = rust_current_module_dir(importer_rel);
        let rel = specifier.replace("::", "/");
        vec![
            normalize_rel(&format!("{base}/{rel}.rs")),
            normalize_rel(&format!("{base}/{rel}/mod.rs")),
        ]
    } else {
        rust_use_candidates(importer_rel, specifier)
    };
    first_known(cands.into_iter(), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "RUST_MODULE",
        confidence: 0.85,
    })
}

fn resolve_dockerfile(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    if kind != "COPY" && kind != "ADD" {
        return None;
    }
    let spec = specifier.trim();
    if spec.is_empty() || spec.contains('$') || spec.contains("://") || spec.starts_with('/') {
        return None;
    }
    let base_dir = Path::new(importer_rel)
        .parent()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    let joined = normalize_rel(&format!("{base_dir}/{spec}"));
    first_known(std::iter::once(joined), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "DOCKER_CONTEXT",
        confidence: 0.9,
    })
}

fn resolve_ruby(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    let cands = ruby_candidates(importer_rel, specifier, kind);
    first_known(cands.into_iter(), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: if kind == "REQUIRE_RELATIVE" {
            "RUBY_RELATIVE"
        } else {
            "RUBY_LOAD_PATH"
        },
        confidence: if kind == "REQUIRE_RELATIVE" { 0.9 } else { 0.8 },
    })
}

fn ruby_candidates(importer_rel: &str, specifier: &str, kind: &str) -> Vec<String> {
    let spec = specifier.replace('\\', "/");
    if kind == "REQUIRE_RELATIVE" || spec.starts_with("./") || spec.starts_with("../") {
        let base_dir = Path::new(importer_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let joined = normalize_rel(&format!("{base_dir}/{spec}"));
        return ruby_file_candidates(joined);
    }

    let normalized = normalize_rel(spec.trim_start_matches('/'));
    let mut out = Vec::new();
    for prefix in ["lib/", "", "app/", "src/"] {
        out.extend(ruby_file_candidates(normalize_rel(&format!(
            "{prefix}{normalized}"
        ))));
    }
    out
}

fn ruby_file_candidates(path: String) -> Vec<String> {
    if path.ends_with(".rb") {
        vec![path]
    } else {
        vec![format!("{path}.rb")]
    }
}

fn resolve_php(
    importer_rel: &str,
    specifier: &str,
    kind: &str,
    known: &BTreeSet<String>,
) -> Option<Upgrade> {
    let cands = if kind.starts_with("REQUIRE") || kind.starts_with("INCLUDE") {
        php_include_candidates(importer_rel, specifier)
    } else {
        php_psr4_candidates(specifier)
    };
    first_known(cands.into_iter(), known).map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: if kind.starts_with("REQUIRE") || kind.starts_with("INCLUDE") {
            "PHP_INCLUDE"
        } else {
            "PHP_PSR4"
        },
        confidence: if kind.starts_with("REQUIRE") || kind.starts_with("INCLUDE") {
            0.9
        } else {
            0.82
        },
    })
}

fn php_include_candidates(importer_rel: &str, specifier: &str) -> Vec<String> {
    let spec = specifier.replace('\\', "/");
    let base = if spec.starts_with('/') {
        normalize_rel(spec.trim_start_matches('/'))
    } else {
        let importer_dir = Path::new(importer_rel)
            .parent()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        normalize_rel(&format!("{importer_dir}/{spec}"))
    };
    if base.ends_with(".php") {
        vec![base]
    } else {
        vec![base.clone(), format!("{base}.php")]
    }
}

fn php_psr4_candidates(specifier: &str) -> Vec<String> {
    let trimmed = specifier.trim_start_matches('\\');
    let parts: Vec<&str> = trimmed
        .split('\\')
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let suffix = parts.join("/");
    let mut lower_parts: Vec<String> = parts.iter().map(|part| (*part).to_string()).collect();
    lower_parts[0] = lower_parts[0].to_ascii_lowercase();
    let lower_suffix = lower_parts.join("/");

    let mut out = Vec::new();
    for prefix in ["", "src/", "app/", "lib/"] {
        out.push(format!("{prefix}{suffix}.php"));
        if lower_suffix != suffix {
            out.push(format!("{prefix}{lower_suffix}.php"));
        }
    }
    out
}

fn resolve_swift(specifier: &str, known: &BTreeSet<String>) -> Option<Upgrade> {
    let module = specifier.split('.').next().unwrap_or(specifier);
    if module.is_empty() {
        return None;
    }
    let dir_prefix = format!("Sources/{module}/");
    let hit = known
        .range(dir_prefix.clone()..)
        .take_while(|path| path.starts_with(&dir_prefix))
        .next()
        .cloned();
    hit.map(|path| Upgrade {
        target: ResolvedTarget::Path(path),
        resolution: ResolutionLevel::PackageResolved,
        basis: "SWIFT_PACKAGE",
        confidence: 0.8,
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
