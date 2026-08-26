//! Phase 3 — Structural Intelligence framework.
//!
//! # Architecture
//!
//! ```text
//! AnalyzerRegistry
//!        |
//! GenericAnalyzer                 Structural layer (this module)
//!        |                              |
//! arbitrary text            Tree-sitter engine (parser mechanics)
//!                                      |
//!                        TreeSitterLanguageSpec adapters (per-language knowledge)
//!                          Java / Python / Go / JavaScript / TypeScript
//!                                      |
//!                          canonical AnalyzerOutput
//!                    (StructuralNodeSpec / SymbolSpec / ImportSpec /
//!                     RelationshipSpec / RetrievalUnitSpec)
//! ```
//!
//! ## Parser-mechanics vs language knowledge
//!
//! Everything mechanical lives in [`engine`] and is reused verbatim by every
//! language: parser creation, grammar registration, parsing, cancellation,
//! budgets (time / AST-node count / recursion depth / memory), node
//! traversal accounting, source-span conversion, malformed-tree handling,
//! diagnostics, and canonical structural-node production.
//!
//! Language-specific behaviour lives exclusively in [`TreeSitterLanguageSpec`]
//! implementations: grammar handle, supported file types, capability
//! declarations, declaration/symbol/import/reference mappings, qualified-name
//! rules and package semantics.
//!
//! ## Adding a language later
//!
//! Register a grammar crate, implement `TreeSitterLanguageSpec`, add fixtures/tests,
//! expose an `analyzer()` factory and list it in [`default_registry`].
//! No storage, indexing, registry, MCP, incremental or GenericAnalyzer code
//! changes are required — proven by
//! `tests/phase3_extensibility.rs` with a mock non-Tree-sitter language.
//!
//! ## Canonical-model boundary
//!
//! `tree_sitter::Node`/`Tree` never escape this module. Storage, indexing,
//! retrieval, MCP and future evidence code depend only on the canonical
//! `AnalyzerOutput` model, which keeps the parser backend replaceable
//! (a future non-Tree-sitter analyzer simply implements `trait Analyzer`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use attic_core::{FileType, SourceSpan, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, RelationshipSpec, ResolutionLevel,
    RetrievalUnitSpec, StructuralNodeSpec, SymbolSpec, diagnostic_codes,
};
use crate::cancellation::CancellationToken;
use crate::generic::GenericAnalyzer;
use crate::registry::AnalyzerRegistry;

pub mod go;
pub mod java;
pub mod javascript;
pub mod python;
pub mod typescript;

/// Streaming prefix cap for LARGE files: bounded structural analysis.
/// The remainder of the file is consumed as plain lexical units so search
/// coverage never silently regresses against the GenericAnalyzer baseline.
const STREAM_PREFIX_CAP_BYTES: usize = 4 * 1024 * 1024;
/// Tail-unit granularity for the streaming remainder.
const TAIL_UNIT_BYTES: usize = 64 * 1024;
/// Budget/deadline/cancellation checks are amortized: perform the relatively
/// expensive clock/atomic reads once per this many bookkeeping operations.
const CHECK_EVERY: u32 = 512;
/// Cap on symbols/imports/relationships collected per file (defensive bound
/// against pathological generated sources; far above any realistic file).
const MAX_ENTITIES_PER_KIND: usize = 20_000;

// ---------------------------------------------------------------------------
// Source text view
// ---------------------------------------------------------------------------

/// Byte view of the content *as delivered to the analyzer* (already redacted
/// when applicable). All spans refer to this representation.
pub(crate) struct SourceText<'a> {
    bytes: &'a [u8],
}

impl<'a> SourceText<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Lossy UTF-8 slice for `[start, end)` — never panics on invalid UTF-8.
    pub fn text(&self, start: usize, end: usize) -> String {
        let end = end.min(self.bytes.len());
        let start = start.min(end);
        String::from_utf8_lossy(&self.bytes[start..end]).into_owned()
    }

    pub fn slice(&self, start: usize, end: usize) -> &[u8] {
        let end = end.min(self.bytes.len());
        let start = start.min(end);
        &self.bytes[start..end]
    }
}

/// Convert a Tree-sitter node span to the canonical 0-based half-open span.
pub(crate) fn span_of(node: Node<'_>) -> SourceSpan {
    let s = node.start_position();
    let e = node.end_position();
    SourceSpan::new(s.row as u32, s.column as u32, e.row as u32, e.column as u32)
}

// ---------------------------------------------------------------------------
// Canonical extraction accumulators (parser-agnostic)
// ---------------------------------------------------------------------------

/// One canonical structural node under construction.
pub(crate) struct CanonNode {
    pub node_type: String,
    pub name: String,
    pub span: SourceSpan,
    pub parent_index: Option<usize>,
    /// Rename-stable identity basis (path-independent, qualified by symbol
    /// ancestry when available). The engine hashes this into the persisted
    /// `structural_identity`.
    pub identity_basis: String,
    pub byte_range: (usize, usize),
}

/// One canonical symbol definition/signature.
pub(crate) struct CanonSymbol {
    pub qualified_name: String,
    pub short_name: String,
    pub kind: SymbolKind,
    pub span: SourceSpan,
    pub is_public: bool,
    pub disambiguator: Option<String>,
    pub signature: Option<String>,
    pub visibility: Option<String>,
    pub is_definition: bool,
    /// Index into `CanonNode` vec this symbol is anchored to.
    pub node_index: Option<usize>,
}

/// One canonical relationship edge.
pub(crate) struct CanonRel {
    pub relationship_type: String,
    pub target: String,
    pub span: SourceSpan,
    pub resolution: ResolutionLevel,
    pub confidence: f64,
    /// Index into `CanonSymbol` vec owning this edge (`None` = file-level,
    /// used for imports and file-scoped edges).
    pub owner_symbol: Option<usize>,
}

/// Accumulator handed to [`TreeSitterLanguageSpec::extract`]. All mutation
/// goes through its methods so resource accounting stays centralized in the
/// engine.
pub(crate) struct Extraction<'a> {
    pub(crate) nodes: Vec<CanonNode>,
    pub(crate) symbols: Vec<CanonSymbol>,
    pub(crate) imports: Vec<crate::api::ImportSpec>,
    pub(crate) rels: Vec<CanonRel>,
    /// `(byte_start, byte_end, node_index)` for every TOP-LEVEL declaration —
    /// drives retrieval-unit segmentation.
    pub(crate) top_level: Vec<(usize, usize, usize)>,
    pub(crate) diagnostics: Vec<AnalyzerDiagnostic>,
    deadline: Instant,
    token: &'a CancellationToken,
    ops: u32,
    pub(crate) stop: bool,
    /// Effective per-entity-kind ceiling: min(hard safety cap,
    /// `ResourceBudget::max_ast_nodes`) so budgets stay reconciled with the
    /// approved resource contract while retaining a hard safety net.
    entity_cap: usize,
    /// Machine-readable record of every truncation that occurred
    /// (`"symbols"`, `"imports"`, `"relationships"`, `"nodes"`, `"time"`,
    /// `"cancelled"`). Non-empty ⇒ output is explicitly PARTIAL.
    pub(crate) truncations: Vec<&'static str>,
    warned_symbols: bool,
    warned_imports: bool,
    warned_rels: bool,
}

impl<'a> Extraction<'a> {
    fn new(deadline: Instant, token: &'a CancellationToken, entity_cap: usize) -> Self {
        Self {
            nodes: Vec::new(),
            symbols: Vec::new(),
            imports: Vec::new(),
            rels: Vec::new(),
            top_level: Vec::new(),
            diagnostics: Vec::new(),
            deadline,
            token,
            ops: 0,
            stop: false,
            entity_cap,
            truncations: Vec::new(),
            warned_symbols: false,
            warned_imports: false,
            warned_rels: false,
        }
    }

    /// Amortized cancellation/deadline poll. Returns `false` when the
    /// extraction must stop immediately.
    pub fn tick(&mut self) -> bool {
        if self.stop {
            return false;
        }
        self.ops += 1;
        if !self.ops.is_multiple_of(CHECK_EVERY) {
            return true;
        }
        if self.token.is_cancelled() {
            self.stop = true;
            self.truncations.push("cancelled");
            self.diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Structural analysis cancelled mid-extraction; output is PARTIAL.",
            ));
            return false;
        }
        if Instant::now() >= self.deadline {
            self.stop = true;
            self.truncations.push("time");
            self.diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                "Time budget exhausted during structural extraction; output is PARTIAL.",
            ));
            return false;
        }
        true
    }

    /// Register a structural node. `parent_index` refers to a previously
    /// pushed node. Returns the new node's index, or `None` when the node
    /// budget is exhausted (truncation recorded).
    pub fn push_node(
        &mut self,
        node_type: &str,
        name: &str,
        node: Node<'_>,
        identity_basis: String,
        parent_index: Option<usize>,
    ) -> Option<usize> {
        if self.nodes.len() >= self.entity_cap {
            if self.truncations.iter().all(|t| *t != "nodes") {
                self.truncations.push("nodes");
                self.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    format!(
                        "node cap ({}) reached; remaining structural nodes skipped — \
                         output is PARTIAL",
                        self.entity_cap
                    ),
                ));
                self.stop = true;
            }
            return None;
        }
        let idx = self.nodes.len();
        self.nodes.push(CanonNode {
            node_type: node_type.to_string(),
            name: name.to_string(),
            span: span_of(node),
            parent_index,
            identity_basis,
            byte_range: (node.start_byte(), node.end_byte()),
        });
        Some(idx)
    }

    /// Mark a just-pushed node as a top-level declaration for unit splitting.
    pub fn mark_top_level(&mut self, node_index: usize) {
        let (s, e) = self.nodes[node_index].byte_range;
        self.top_level.push((s, e, node_index));
    }

    /// Register a symbol definition/signature. Truncates at the effective
    /// entity cap with an observable diagnostic (warned once per kind).
    pub fn push_symbol(&mut self, sym: CanonSymbol) {
        if self.stop {
            return;
        }
        if self.symbols.len() >= self.entity_cap {
            if !self.warned_symbols {
                self.warned_symbols = true;
                self.truncations.push("symbols");
                self.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    format!(
                        "symbol cap ({}) reached; further symbols skipped — output is PARTIAL",
                        self.entity_cap
                    ),
                ));
            }
            return;
        }
        self.symbols.push(sym);
    }

    /// Register an import edge (budget-aware, observable truncation).
    pub fn push_import(&mut self, raw_specifier: String, import_kind: &str, node: Node<'_>) {
        if self.stop {
            return;
        }
        if self.imports.len() >= self.entity_cap {
            if !self.warned_imports {
                self.warned_imports = true;
                self.truncations.push("imports");
                self.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    format!(
                        "import cap ({}) reached; further imports skipped — output is PARTIAL",
                        self.entity_cap
                    ),
                ));
            }
            return;
        }
        self.imports.push(crate::api::ImportSpec {
            raw_specifier,
            resolved_path: None,
            span: span_of(node),
            import_kind: import_kind.to_string(),
        });
    }

    /// Register a relationship edge (budget-aware, observable truncation).
    pub fn push_rel(
        &mut self,
        relationship_type: &str,
        target: String,
        node: Node<'_>,
        resolution: ResolutionLevel,
        confidence: f64,
        owner_symbol: Option<usize>,
    ) {
        if self.stop {
            return;
        }
        if self.rels.len() >= self.entity_cap {
            if !self.warned_rels {
                self.warned_rels = true;
                self.truncations.push("relationships");
                self.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    format!(
                        "relationship cap ({}) reached; further edges skipped — output is PARTIAL",
                        self.entity_cap
                    ),
                ));
            }
            return;
        }
        self.rels.push(CanonRel {
            relationship_type: relationship_type.to_string(),
            target,
            span: span_of(node),
            resolution,
            confidence,
            owner_symbol,
        });
    }
}

// ---------------------------------------------------------------------------
// TreeSitterLanguageSpec — the Tree-sitter BACKEND adapter trait
// ---------------------------------------------------------------------------

/// Per-language knowledge supplied to the shared Tree-sitter engine.
///
/// **Backend boundary.** This trait is deliberately Tree-sitter-specific and
/// `pub(crate)`: it is the contract between the bundled TS backend and its
/// language adapters — nothing else. The PUBLIC extension point for ANY
/// parser backend (Tree-sitter, a compiler/native API, an external protocol
/// analyzer, …) is [`crate::Analyzer`] itself: implement it, register via
/// [`crate::AnalyzerRegistry::register_specialized`], and storage /
/// indexing / MCP / incremental code is untouched (proven by
/// `tests/phase3_extensibility.rs`). Built-in language enumeration in
/// [`default_registry`] is a composition-root concern only; central dispatch
/// contains no per-language branching.
///
/// # Contract
///
/// - `extract` MUST be deterministic for identical input.
/// - `extract` MUST NOT panic on malformed trees (error nodes are expected).
/// - All entity registration goes through `out` so budgets/cancellation are
///   enforced centrally. Check `out.tick()` in loops and bail when `false`.
/// - Never fabricate resolution levels above actual evidence.
pub(crate) trait TreeSitterLanguageSpec: Send + Sync {
    /// Stable analyzer id (e.g. `"java-treesitter"`); never changes.
    fn analyzer_id(&self) -> &'static str;
    /// Human-readable description for the descriptor.
    fn description(&self) -> &'static str;
    /// File types claimed by this language.
    fn file_types(&self) -> &'static [FileType];
    /// Capability matrix advertised independently per language.
    fn capabilities(&self) -> AnalyzerCapabilities;
    /// Grammar handle (bundled; see ADR-010 for pinned versions/licenses).
    fn grammar(&self) -> tree_sitter_language::LanguageFn;
    /// Language name recorded on symbol identities (`core_symbol_identities.language`).
    fn language_tag(&self) -> &'static str;

    /// Extract canonical structure from the parsed tree rooted at `root`.
    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>);
}

// ---------------------------------------------------------------------------
// Shared Tree-sitter engine (parser mechanics)
// ---------------------------------------------------------------------------

/// Deterministic BLAKE3-based `structural_identity` from a rename-stable basis.
fn structural_identity(basis: &str) -> String {
    blake3::hash(basis.as_bytes()).to_hex().to_string()
}

/// Assign deterministic disambiguators to duplicate `(qualified_name, kind)`
/// definitions (e.g. Java/TypeScript overloads): first keeps `None`,
/// subsequent duplicates get `"overload:N"` ordered by span position.
fn assign_disambiguators(symbols: &mut [CanonSymbol]) {
    use std::collections::HashMap;
    let mut seen: HashMap<(String, String), u32> = HashMap::new();
    // Sort candidate order by span for stable numbering: iterate in span order
    // via index sort, then write back.
    let mut order: Vec<usize> = (0..symbols.len()).collect();
    order.sort_by_key(|&i| {
        let s = &symbols[i].span;
        (s.start_line, s.start_col, s.end_line, s.end_col)
    });
    for &i in &order {
        let key = (
            symbols[i].qualified_name.clone(),
            format!("{:?}", symbols[i].kind),
        );
        let n = seen.entry(key).or_insert(0);
        *n += 1;
        if *n > 1 {
            symbols[i].disambiguator = Some(format!("overload:{n}"));
        }
    }
}

pub(crate) mod engine {
    use super::*;

    /// Run the shared engine for `spec` over `input`. Never panics on
    /// repository input; fatal conditions surface as `Error` diagnostics so
    /// the dispatcher falls back to `GenericAnalyzer` (contract §Failure).
    pub fn run(spec: &dyn TreeSitterLanguageSpec, mut input: AnalyzerInput) -> AnalyzerOutput {
        let started = Instant::now();
        let deadline = started
            + Duration::from_millis(input.resource_budget.max_time_ms.min(u64::from(u32::MAX)));
        let mut diags: Vec<AnalyzerDiagnostic> = Vec::new();

        if input.cancellation_token.is_cancelled() {
            return partial_output(
                spec,
                &input,
                vec![AnalyzerDiagnostic::warning(
                    diagnostic_codes::CANCELLED,
                    "Cancelled before structural analysis started.",
                )],
            );
        }

        // ── 1. Acquire bounded content ──────────────────────────────────────
        let mut streamed_tail: Option<Vec<String>> = None;
        let mut prefix_truncated = false;
        let bytes: Vec<u8> =
            match std::mem::replace(&mut input.content, AnalyzerContent::FullBytes(Vec::new())) {
                AnalyzerContent::FullBytes(b) => b,
                AnalyzerContent::RedactedBytes(b) => b,
                AnalyzerContent::StreamingHandle(ref mut stream) => {
                    // Bounded prefix for structural parsing…
                    let mut prefix: Vec<u8> = Vec::with_capacity(64 * 1024);
                    loop {
                        if input.cancellation_token.is_cancelled() || Instant::now() >= deadline {
                            break;
                        }
                        match stream.next_chunk() {
                            None => break,
                            Some(Err(_)) => break,
                            Some(Ok(chunk)) => {
                                prefix.extend_from_slice(chunk.redacted.as_bytes());
                                if prefix.len() >= STREAM_PREFIX_CAP_BYTES {
                                    // …and the remainder becomes lexical units.
                                    let mut tail = Vec::new();
                                    let mut buf = String::new();
                                    while let Some(next) = stream.next_chunk() {
                                        match next {
                                            Err(_) => break,
                                            Ok(c) => {
                                                buf.push_str(&c.redacted);
                                                if buf.len() >= TAIL_UNIT_BYTES {
                                                    tail.push(std::mem::take(&mut buf));
                                                }
                                            }
                                        }
                                    }
                                    if !buf.is_empty() {
                                        tail.push(buf);
                                    }
                                    streamed_tail = Some(tail);
                                    prefix_truncated = true;
                                    diags.push(AnalyzerDiagnostic::warning(
                                        "STRUCTURAL_TRUNCATED",
                                        "LARGE file: structural analysis covered the first \
                                     ~4 MiB; remaining content indexed as lexical units.",
                                    ));
                                    break;
                                }
                            }
                        }
                    }
                    prefix
                }
            };
        if input.cancellation_token.is_cancelled() {
            return partial_output(
                spec,
                &input,
                vec![AnalyzerDiagnostic::warning(
                    diagnostic_codes::CANCELLED,
                    "Cancelled during content acquisition.",
                )],
            );
        }

        let src = SourceText::new(&bytes);

        // ── 2. Parse ─────────────────────────────────────────────────────────
        let mut parser = match make_parser(spec.grammar()) {
            Ok(p) => p,
            Err(msg) => {
                // Fatal: cannot even load the grammar — full fallback.
                diags.push(AnalyzerDiagnostic::error(
                    "PARSE_INIT_FAILED",
                    format!("failed to initialize parser: {msg}"),
                ));
                return fatal(spec, &input, diags);
            }
        };
        let tree = parser.parse(&bytes, None);
        let Some(tree) = tree else {
            diags.push(AnalyzerDiagnostic::error(
                "PARSE_FAILED",
                "parser produced no tree".to_string(),
            ));
            return fatal(spec, &input, diags);
        };
        let root = tree.root_node();
        if root.has_error() {
            // Partially parses → usable supported output + diagnostics.
            diags.push(AnalyzerDiagnostic::warning(
                "PARSE_ERROR",
                "source contains syntax errors; structural output is partial",
            ));
        }

        // ── 3. AST budget pre-pass: node count AND depth ─────────────────────
        // Depth is enforced as a HARD ceiling (fatal → fallback): language
        // extractors recurse over the tree, so a tree deeper than
        // `max_recursion_depth` must never reach them. This keeps the
        // recursion guard aligned with `ResourceBudget` instead of an
        // invisible engine constant.
        let counted = count_nodes(
            root,
            input.resource_budget.max_ast_nodes,
            input.resource_budget.max_recursion_depth,
        );
        if counted.overflowed {
            diags.push(AnalyzerDiagnostic::error(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "AST exceeds budget ({} nodes > max_ast_nodes {})",
                    counted.visited, input.resource_budget.max_ast_nodes
                ),
            ));
            return fatal(spec, &input, diags);
        }
        if !counted.depth_overflow.is_empty() {
            diags.push(AnalyzerDiagnostic::error(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "AST nesting exceeds max_recursion_depth ({}) at node kinds {:?}; \
                     refusing unbounded recursive extraction",
                    input.resource_budget.max_recursion_depth, counted.depth_overflow
                ),
            ));
            return fatal(spec, &input, diags);
        }

        // ── 4. Language extraction ───────────────────────────────────────────
        // Effective entity ceiling reconciles the hard safety cap with the
        // approved per-file budget: `max_retrieval_units` bounds emitted
        // output size, so entities (nodes/symbols/imports/edges) share that
        // ceiling rather than an invisible constant. The hard safety cap
        // remains for defence-in-depth.
        let entity_cap = MAX_ENTITIES_PER_KIND.min(
            usize::try_from(input.resource_budget.max_retrieval_units)
                .unwrap_or(MAX_ENTITIES_PER_KIND),
        );
        let mut ex = Extraction::new(deadline, &input.cancellation_token, entity_cap.max(1));
        if prefix_truncated {
            ex.truncations.push("prefix");
        }
        spec.extract(root, &src, &mut ex);
        diags.append(&mut ex.diagnostics);

        // Post-process: deterministic overload disambiguation.
        assign_disambiguators(&mut ex.symbols);

        // ── 5. Canonical conversion ──────────────────────────────────────────
        let structural_nodes: Vec<StructuralNodeSpec> = ex
            .nodes
            .iter()
            .map(|n| StructuralNodeSpec {
                node_type: n.node_type.clone(),
                name: n.name.clone(),
                span: n.span,
                parent_index: n.parent_index,
                structural_identity: structural_identity(&n.identity_basis),
                content_hash: blake3::hash(src.slice(n.byte_range.0, n.byte_range.1))
                    .to_hex()
                    .to_string(),
                metadata_json: None,
            })
            .collect();

        let symbols: Vec<SymbolSpec> = ex
            .symbols
            .iter()
            .map(|s| SymbolSpec {
                qualified_name: s.qualified_name.clone(),
                short_name: s.short_name.clone(),
                kind: s.kind,
                definition_span: s.span,
                is_public: s.is_public,
                disambiguator: s.disambiguator.clone(),
                signature: s.signature.clone(),
                visibility: s.visibility.clone(),
                is_definition: s.is_definition,
                node_index: s.node_index,
            })
            .collect();

        let relationships: Vec<RelationshipSpec> = ex
            .rels
            .iter()
            .map(|r| RelationshipSpec {
                relationship_type: r.relationship_type.clone(),
                target_qualified_name: r.target.clone(),
                span: r.span,
                resolution: r.resolution,
                confidence: r.confidence,
                source_symbol_index: r.owner_symbol,
            })
            .collect();

        // ── 6. Retrieval units: declaration segments + gap coverage ──────────
        let mut units = build_units(&src, &ex.top_level, &ex.nodes, &streamed_tail);
        let max_units = input.resource_budget.max_retrieval_units.max(1);
        if units.len() as u64 > max_units {
            units.truncate(max_units as usize);
            diags.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                "retrieval-unit cap reached; output truncated",
            ));
        }

        let capability_used = capability_used_for(&ex);

        // §4/§5: ANY truncation makes the structural result explicitly
        // PARTIAL — never presented as complete.
        let structurally_complete = ex.truncations.is_empty();

        AnalyzerOutput {
            analyzer_id: spec.analyzer_id().to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes,
            symbols,
            imports: ex.imports,
            relationships,
            retrieval_units: units,
            diagnostics: diags,
            fallback_used: false,
            structurally_complete,
            capability_used,
        }
    }

    fn capability_used_for(ex: &Extraction<'_>) -> CapabilityKind {
        // Report the richest capability actually exercised (orthogonal axes —
        // never inferred from ordinals alone).
        if !ex.rels.is_empty() {
            CapabilityKind::ReferenceExtraction
        } else if !ex.imports.is_empty() {
            CapabilityKind::ImportExtraction
        } else if !ex.symbols.is_empty() {
            CapabilityKind::SymbolExtraction
        } else {
            CapabilityKind::StructuralParse
        }
    }

    /// Iterative node count with depth measurement. Stops early once `limit`
    /// is exceeded so hostile inputs cost O(limit). Records the kinds of the
    /// first nodes found BEYOND `max_depth` so the refusal is diagnosable.
    fn count_nodes(root: Node<'_>, limit: u64, max_depth: u32) -> CountResult {
        let mut visited: u64 = 0;
        let mut stack: Vec<(Node<'_>, u32)> = vec![(root, 0)];
        let mut depth_overflow: Vec<&'static str> = Vec::new();
        while let Some((node, depth)) = stack.pop() {
            visited += 1;
            if visited > limit {
                return CountResult {
                    visited,
                    overflowed: true,
                    depth_overflow: std::mem::take(&mut depth_overflow),
                };
            }
            if depth > max_depth {
                if depth_overflow.len() < 4 && !depth_overflow.contains(&node.kind()) {
                    // SAFETY-free leak of a 'static str from kind(): node
                    // kinds are &'static str by tree-sitter's API.
                    depth_overflow.push(node.kind());
                }
                continue; // do not descend past the ceiling
            }
            let mut c = node.walk();
            for child in node.children(&mut c) {
                stack.push((child, depth + 1));
            }
        }
        CountResult {
            visited,
            overflowed: false,
            depth_overflow,
        }
    }

    struct CountResult {
        visited: u64,
        overflowed: bool,
        /// Kinds of the first nodes found beyond the recursion ceiling.
        depth_overflow: Vec<&'static str>,
    }

    fn partial_output(
        spec: &dyn TreeSitterLanguageSpec,
        input: &AnalyzerInput,
        diags: Vec<AnalyzerDiagnostic>,
    ) -> AnalyzerOutput {
        AnalyzerOutput {
            analyzer_id: spec.analyzer_id().to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![],
            imports: vec![],
            relationships: vec![],
            retrieval_units: vec![],
            diagnostics: diags,
            fallback_used: false,
            structurally_complete: false,
            capability_used: CapabilityKind::Lexical,
        }
    }

    /// Fatal failure output: carries `Error` diagnostics so the dispatcher
    /// routes to `GenericAnalyzer` (file remains fully searchable).
    fn fatal(
        spec: &dyn TreeSitterLanguageSpec,
        input: &AnalyzerInput,
        diags: Vec<AnalyzerDiagnostic>,
    ) -> AnalyzerOutput {
        AnalyzerOutput {
            analyzer_id: spec.analyzer_id().to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![],
            imports: vec![],
            relationships: vec![],
            retrieval_units: vec![],
            diagnostics: diags,
            fallback_used: false,
            structurally_complete: false,
            capability_used: CapabilityKind::Lexical,
        }
    }

    /// Segment the delivered content into retrieval units:
    /// prologue gap → per-declaration units (linked to their node) →
    /// inter/post gaps merged → optional streaming tail units.
    fn build_units(
        src: &SourceText<'_>,
        top_level: &[(usize, usize, usize)],
        nodes: &[CanonNode],
        tail: &Option<Vec<String>>,
    ) -> Vec<RetrievalUnitSpec> {
        let mut units = Vec::new();
        let total = src.len();
        let mut ordinal: u32 = 0;
        let mut cursor = 0usize;

        let emit_gap =
            |start: usize, end: usize, units: &mut Vec<RetrievalUnitSpec>, ordinal: &mut u32| {
                if end <= start {
                    return;
                }
                let text = src.text(start, end);
                if text.trim().is_empty() {
                    return;
                }
                units.push(RetrievalUnitSpec {
                    span: span_for_bytes(start, end, src),
                    retrieval_text: text,
                    ordinal: *ordinal,
                    structural_node_index: None,
                });
                *ordinal += 1;
            };

        for &(start, end, node_idx) in top_level {
            if start > cursor {
                emit_gap(cursor, start, &mut units, &mut ordinal);
            }
            let end_clamped = end.min(total);
            let text = src.text(start, end_clamped);
            units.push(RetrievalUnitSpec {
                span: nodes[node_idx].span,
                retrieval_text: text,
                ordinal,
                structural_node_index: Some(node_idx),
            });
            ordinal += 1;
            cursor = cursor.max(end_clamped);
        }
        if cursor < total {
            emit_gap(cursor, total, &mut units, &mut ordinal);
        }

        if units.is_empty() && total > 0 {
            // No recognizable declarations: fall back to coarse chunks so the
            // specialized path never indexes less than GenericAnalyzer would.
            const CHUNK: usize = 400 * 80; // ~lines worth of bytes
            let mut pos = 0usize;
            while pos < total {
                let end = (pos + CHUNK).min(total);
                units.push(RetrievalUnitSpec {
                    span: span_for_bytes(pos, end, src),
                    retrieval_text: src.text(pos, end),
                    ordinal,
                    structural_node_index: None,
                });
                ordinal += 1;
                pos = end;
            }
        }

        if let Some(tail_chunks) = tail {
            for chunk_text in tail_chunks {
                units.push(RetrievalUnitSpec {
                    span: SourceSpan::new(0, 0, 0, 0),
                    retrieval_text: chunk_text.clone(),
                    ordinal,
                    structural_node_index: None,
                });
                ordinal += 1;
            }
        }

        units
    }

    /// Approximate line span for a byte range without re-tokenizing.
    fn span_for_bytes(start: usize, end: usize, src: &SourceText<'_>) -> SourceSpan {
        let line_of = |idx: usize| -> u32 {
            let mut line = 0u32;
            let bytes = src.slice(0, idx.min(src.len()));
            for b in bytes {
                if *b == b'\n' {
                    line += 1;
                }
            }
            line
        };
        SourceSpan::new(line_of(start), 0, line_of(end), 0)
    }

    /// Create a parser bound to `grammar`, reporting failures as strings so
    /// the engine can degrade to fallback instead of panicking.
    fn make_parser(
        grammar: tree_sitter_language::LanguageFn,
    ) -> Result<tree_sitter::Parser, String> {
        let language: tree_sitter::Language = grammar.into();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).map_err(|e| e.to_string())?;
        Ok(parser)
    }
}

// ---------------------------------------------------------------------------
// Public analyzer wrapper
// ---------------------------------------------------------------------------

/// A `TreeSitterLanguageSpec` exposed through the Phase 1C `Analyzer` contract.
///
/// The wrapper is intentionally thin: all behaviour comes from the spec plus
/// the shared engine. Panic safety is provided by `dispatch`'s
/// `catch_unwind`; this type additionally avoids panicking on its own.
pub struct TreeSitterAnalyzer {
    descriptor: AnalyzerDescriptor,
    inner: &'static dyn TreeSitterLanguageSpec,
}

impl TreeSitterAnalyzer {
    fn new(inner: &'static dyn TreeSitterLanguageSpec) -> Self {
        let descriptor = AnalyzerDescriptor {
            name: inner.analyzer_id().to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: inner.description().to_string(),
            supported_file_types: inner.file_types().to_vec(),
            capabilities: inner.capabilities(),
        };
        Self { descriptor, inner }
    }

    /// Language tag recorded on symbol identities
    /// (`core_symbol_identities.language`), e.g. `"java"`.
    pub fn language_tag(&self) -> &'static str {
        self.inner.language_tag()
    }
}

impl Analyzer for TreeSitterAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.descriptor
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        engine::run(self.inner, input)
    }
}

// ---------------------------------------------------------------------------
// Registry wiring
// ---------------------------------------------------------------------------

/// Build the standard registry: `GenericAnalyzer` fallback plus every bundled
/// structural language. Adding a language = one line here (plus its spec
/// module); no other subsystem changes.
///
/// Note: callers that need a *custom* registry (tests, future plugin hosts)
/// can compose `AnalyzerRegistry` themselves — this helper is convenience,
/// not a coupling point.
pub fn default_registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(java::analyzer());
    reg.register_specialized(python::analyzer());
    reg.register_specialized(go::analyzer());
    reg.register_specialized(javascript::analyzer());
    reg.register_specialized(typescript::analyzer());
    reg
}

/// Shared helper for language modules: construct the public analyzer.
pub(crate) fn make_analyzer(spec: &'static dyn TreeSitterLanguageSpec) -> Arc<dyn Analyzer> {
    Arc::new(TreeSitterAnalyzer::new(spec))
}
