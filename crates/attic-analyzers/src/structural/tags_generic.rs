//! Tier 2 — generic, tags.scm-based structural analyzer.
//!
//! Unlike [`super::TreeSitterLanguageSpec`] (hand-written AST walking, one
//! module per language), this module implements [`crate::Analyzer`]
//! **directly**, once, and is parameterized per language by a small
//! data-driven [`TagsLanguageSpec`] table. Each language's grammar crate
//! bundles its own `tags.scm` (the same convention GitHub/Neovim/Helix use
//! for cross-language "go to definition") — this engine simply consumes it.
//!
//! ## Coverage
//!
//! C, C++, Ruby, C#, Scala, PHP, Swift, Lua, Rust — 9 off-the-shelf grammars —
//! plus Dockerfile via one hand-authored query (10 languages total). See
//! [`tier2_table`] for the exact table.
//!
//! **Kotlin — deviation from the original plan.** The plan listed Kotlin
//! among the off-the-shelf tier-2 languages. Verified against the actual
//! pinned crate (`tree-sitter-kotlin-ng` 1.1.0) and its upstream repository
//! (`tree-sitter-grammars/tree-sitter-kotlin` at the exact published commit,
//! confirmed via the GitHub API): neither ships **any** `queries/` directory
//! at all — not `tags.scm`, not even `highlights.scm`. There is no upstream
//! query to consume, unlike Dockerfile (whose grammar author simply never
//! wrote a `tags.scm`, but whose grammar is simple enough for us to author
//! one by hand — a multi-stage-build stage name is the only meaningful
//! def/ref pair). Hand-authoring a full tags query for a general-purpose
//! language grammar we did not write is a materially different, much larger
//! undertaking than the few-line Dockerfile query, and out of scope for this
//! pass. Kotlin `.kt`/`.kts` files therefore continue to get flat full-text
//! (tier 3) coverage only, same as before this change.
//!
//! ## Honest capability declaration
//!
//! `StructuralParse=Full`, `SymbolExtraction=Basic`, `ImportExtraction=None`,
//! `ReferenceExtraction=None`, `RelationshipResolution=None`.
//!
//! **Deviation from the original plan's suggested table** (which listed
//! `ReferenceExtraction=Basic`): `tags.scm` also yields `@reference.*` tags
//! (e.g. call sites), but this engine does not store them anywhere in
//! [`crate::api::AnalyzerOutput`] — not as [`crate::api::SymbolSpec`] entries
//! (whose `is_definition=false` case is documented elsewhere as "a signature
//! without a body", a different concept from "a reference occurred here"),
//! and not as [`crate::api::RelationshipSpec`] entries (which the plan itself
//! says to avoid fabricating without a resolved target: "it may be more
//! honest to skip populating relationships entirely for tier 2"). Since no
//! reference-related artifact is actually produced, declaring
//! `ReferenceExtraction=Basic` would overclaim; `None` is the honest level
//! for what this engine currently does. Only `@definition.*` tags become
//! symbols/structural nodes.
//!
//! ## Retrieval units
//!
//! Rather than re-implementing gap-filling/chunking, this engine delegates
//! retrieval-unit production entirely to [`crate::generic::GenericAnalyzer`]
//! (same content, run first) and overlays structural nodes/symbols from the
//! tags pass on top. This guarantees tier-2 languages never index *less*
//! than the generic fallback would, and reuses already-tested
//! budget/cancellation/redaction/streaming handling verbatim instead of
//! duplicating it.
//!
//! LARGE (streamed) files are not given tags-based structural treatment —
//! only `GenericAnalyzer`'s lexical output is returned, honestly marked. The
//! hand-written tier-1 engine's bounded-streaming-prefix strategy exists
//! because tier 1 is the premium tier; duplicating that complexity for a
//! "cheap, off-the-shelf" tier is not justified. Lexical search coverage is
//! never lost either way.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use attic_core::SymbolKind;
use tree_sitter_tags::{TagsConfiguration, TagsContext};

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, StructuralNodeSpec, SymbolSpec,
    diagnostic_codes,
};
use crate::generic::GenericAnalyzer;

/// Hard safety cap on symbols/structural nodes collected per file (defensive
/// bound against pathological generated sources), reconciled with the
/// per-invocation `ResourceBudget` the same way the tier-1 engine does.
const TAGS_ENTITY_CAP: usize = 20_000;
/// Amortized cancellation/deadline poll interval (tags processed between checks).
const CHECK_EVERY: u32 = 256;

// ---------------------------------------------------------------------------
// Hand-authored Dockerfile tags query
// ---------------------------------------------------------------------------

/// `camdencheek/tree-sitter-dockerfile` (published as `tree-sitter-containerfile`)
/// ships no `tags.scm` upstream (confirmed: its `queries/` directory contains
/// only `highlights.scm`/`injections.scm`). Dockerfile's grammar is simple
/// enough that the only meaningful definition/reference pair — multi-stage
/// build stage names (`FROM x AS name` / `--from=name`) — can be authored
/// directly. Verified against a real multi-stage sample; see
/// `tests/structural_tags_tier2.rs`.
const DOCKERFILE_TAGS_QUERY: &str = r#"
(from_instruction as: (image_alias) @name) @definition.module
(from_instruction (image_spec) @name) @reference.module
"#;

// ---------------------------------------------------------------------------
// Scala tags query — inlined verbatim from the pinned crate's own source
// ---------------------------------------------------------------------------

/// `tree-sitter-scala` 0.26.2 bundles a real `queries/tags.scm` (confirmed by
/// inspecting the crate's extracted source under
/// `~/.cargo/registry/src/.../tree-sitter-scala-0.26.2/queries/tags.scm`) but,
/// unlike every other grammar crate in this table, its `bindings/rust/lib.rs`
/// does not re-export it as a `pub const TAGS_QUERY`. Copied verbatim (not
/// re-derived/guessed) from that exact file at that exact pinned version so
/// it stays byte-for-byte in sync with the grammar it is queried against.
const SCALA_TAGS_QUERY: &str = r#"
; Definitions

(package_clause
  name: (package_identifier) @name) @definition.module

(trait_definition
  name: (identifier) @name) @definition.interface

(enum_definition
  name: (identifier) @name) @definition.enum

(simple_enum_case
  name: (identifier) @name) @definition.class

(full_enum_case
  name: (identifier) @name) @definition.class

(class_definition
  name: (identifier) @name) @definition.class

(object_definition
  name: (identifier) @name) @definition.object

(function_definition
  name: (identifier) @name) @definition.function

(val_definition
  pattern: (identifier) @name) @definition.variable

(given_definition
  name: (identifier) @name) @definition.variable

(var_definition
  pattern: (identifier) @name) @definition.variable

(val_declaration
  name: (identifier) @name) @definition.variable

(var_declaration
  name: (identifier) @name) @definition.variable

(type_definition
  name: (type_identifier) @name) @definition.type

(class_parameter
  name: (identifier) @name) @definition.property

; References

(call_expression
  (identifier) @name) @reference.call

(instance_expression
  (type_identifier) @name) @reference.interface

(instance_expression
  (generic_type
    (type_identifier) @name)) @reference.interface

(extends_clause
  (type_identifier) @name) @reference.class

(extends_clause
  (generic_type
    (type_identifier) @name)) @reference.class
"#;

// ---------------------------------------------------------------------------
// C# tags query — inlined, minus one incompatible trailing pattern
// ---------------------------------------------------------------------------

/// `tree-sitter-c-sharp` 0.23.5's bundled `queries/tags.scm` ends with one
/// pattern using a bare `@module` capture (no `definition.`/`reference.`
/// prefix): `(namespace_declaration name: (identifier) @name) @module`.
/// `tree-sitter-tags` 0.26's `TagsConfiguration::new` rejects any capture
/// name that isn't `@definition.*`, `@reference.*`, `@doc`, `@name`, or
/// `@local.(scope|definition|reference)` — confirmed empirically: passing
/// the crate's own unmodified `TAGS_QUERY` constant fails with
/// `Error::InvalidCapture("module")`. The pattern is also redundant: the one
/// immediately above it already captures the identical
/// `namespace_declaration` name as `@definition.module`. Inlined here
/// verbatim from the crate's exact bundled query, minus only that one
/// trailing (invalid, redundant) pattern — not a hand-authored query, and no
/// coverage is lost since the duplicate capture is dropped, not the data.
const CSHARP_TAGS_QUERY: &str = r#"
(class_declaration name: (identifier) @name) @definition.class

(class_declaration (base_list (_) @name)) @reference.class

(interface_declaration name: (identifier) @name) @definition.interface

(interface_declaration (base_list (_) @name)) @reference.interface

(method_declaration name: (identifier) @name) @definition.method

(object_creation_expression type: (identifier) @name) @reference.class

(type_parameter_constraints_clause (identifier) @name) @reference.class

(type_parameter_constraint (type type: (identifier) @name)) @reference.class

(variable_declaration type: (identifier) @name) @reference.class

(invocation_expression function: (member_access_expression name: (identifier) @name)) @reference.send

(namespace_declaration name: (identifier) @name) @definition.module
"#;

// ---------------------------------------------------------------------------
// Data-driven language table
// ---------------------------------------------------------------------------

/// One row per tags.scm-backed language. Adding a language later is one row
/// here (plus, if needed, its grammar/query dependency) — no other code
/// changes required.
struct TagsLanguageSpec {
    /// Stable analyzer id (e.g. `"c-tags"`); never changes.
    analyzer_id: &'static str,
    /// Language tag recorded on symbol identities and used as the
    /// `AnalyzerRegistry::register_for_language` key. Must match the tag
    /// `attic-indexing::infer_language_hint` emits for this language's
    /// file extensions exactly.
    language_tag: &'static str,
    /// Human-readable description for the descriptor.
    description: &'static str,
    /// Grammar handle (bundled; matches the pinned crate versions in
    /// `Cargo.toml`).
    grammar: tree_sitter_language::LanguageFn,
    /// The language's `tags.scm` query text.
    tags_query: &'static str,
    /// The language's `locals.scm` query text, or `""` if unavailable.
    locals_query: &'static str,
}

/// The full tier-2 table. See module docs for the Kotlin exclusion.
fn tier2_table() -> Vec<TagsLanguageSpec> {
    vec![
        TagsLanguageSpec {
            analyzer_id: "c-tags",
            language_tag: "c",
            description: "tree-sitter-tags structural analyzer for C: symbols via the \
                grammar's bundled tags.scm (structs/unions, functions, typedefs, enums).",
            grammar: tree_sitter_c::LANGUAGE,
            tags_query: tree_sitter_c::TAGS_QUERY,
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "cpp-tags",
            language_tag: "cpp",
            description: "tree-sitter-tags structural analyzer for C++: symbols via the \
                grammar's bundled tags.scm (structs/unions/classes, functions, methods, \
                typedefs, enums).",
            grammar: tree_sitter_cpp::LANGUAGE,
            tags_query: tree_sitter_cpp::TAGS_QUERY,
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "ruby-tags",
            language_tag: "ruby",
            description: "tree-sitter-tags structural analyzer for Ruby: symbols via the \
                grammar's bundled tags.scm (methods, classes, modules).",
            grammar: tree_sitter_ruby::LANGUAGE,
            tags_query: tree_sitter_ruby::TAGS_QUERY,
            locals_query: tree_sitter_ruby::LOCALS_QUERY,
        },
        TagsLanguageSpec {
            analyzer_id: "csharp-tags",
            language_tag: "csharp",
            description: "tree-sitter-tags structural analyzer for C#: symbols via the \
                grammar's bundled tags.scm (classes, interfaces, methods, namespaces).",
            grammar: tree_sitter_c_sharp::LANGUAGE,
            tags_query: CSHARP_TAGS_QUERY,
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "scala-tags",
            language_tag: "scala",
            description: "tree-sitter-tags structural analyzer for Scala: symbols via the \
                grammar's own tags.scm, inlined (see module docs) since the published crate \
                does not re-export it as a Rust constant (packages, traits, enums, classes, \
                objects, functions, vals/vars, type definitions).",
            grammar: tree_sitter_scala::LANGUAGE,
            tags_query: SCALA_TAGS_QUERY,
            locals_query: tree_sitter_scala::LOCALS_QUERY,
        },
        TagsLanguageSpec {
            analyzer_id: "php-tags",
            language_tag: "php",
            description: "tree-sitter-tags structural analyzer for PHP: symbols via the \
                grammar's bundled tags.scm (namespaces, interfaces, traits, classes, \
                functions, methods).",
            grammar: tree_sitter_php::LANGUAGE_PHP,
            tags_query: tree_sitter_php::TAGS_QUERY,
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "swift-tags",
            language_tag: "swift",
            description: "tree-sitter-tags structural analyzer for Swift: symbols via the \
                grammar's bundled tags.scm (classes, protocols, methods, properties, \
                functions).",
            grammar: tree_sitter_swift::LANGUAGE,
            tags_query: tree_sitter_swift::TAGS_QUERY,
            // NOT `tree_sitter_swift::LOCALS_QUERY`: its bundled `locals.scm`
            // uses suffixed capture names (`@local.definition.import`,
            // `@local.definition.function`) that `tree-sitter-tags` 0.26
            // rejects — only the exact name `@local.definition` is
            // recognized, confirmed empirically (`Error::InvalidCapture`).
            // Swift's `tags.scm` itself has no `local.*` captures, so
            // omitting the locals query changes nothing about definition
            // extraction; it only forgoes local-vs-global reference
            // disambiguation, which this engine does not use anyway (no
            // reference tags are represented in the output — see module docs).
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "lua-tags",
            language_tag: "lua",
            description: "tree-sitter-tags structural analyzer for Lua: symbols via the \
                grammar's bundled tags.scm (functions, methods).",
            grammar: tree_sitter_lua::LANGUAGE,
            tags_query: tree_sitter_lua::TAGS_QUERY,
            locals_query: tree_sitter_lua::LOCALS_QUERY,
        },
        TagsLanguageSpec {
            analyzer_id: "rust-tags",
            language_tag: "rust",
            description: "tree-sitter-tags structural analyzer for Rust: symbols via the \
                grammar's bundled tags.scm (structs/enums/unions, functions, methods, \
                traits, modules, macros).",
            grammar: tree_sitter_rust::LANGUAGE,
            tags_query: tree_sitter_rust::TAGS_QUERY,
            locals_query: "",
        },
        TagsLanguageSpec {
            analyzer_id: "dockerfile-tags",
            language_tag: "dockerfile",
            description: "tree-sitter-tags structural analyzer for Dockerfile/Containerfile: \
                multi-stage build stage names, via a hand-authored tags query (see module \
                docs — no upstream tags.scm exists for this grammar).",
            grammar: tree_sitter_containerfile::LANGUAGE,
            tags_query: DOCKERFILE_TAGS_QUERY,
            locals_query: "",
        },
    ]
}

/// Honest capability declaration shared by every tier-2 language. See module
/// docs for why `ReferenceExtraction`/`RelationshipResolution` are `None`.
fn tier2_capabilities() -> AnalyzerCapabilities {
    AnalyzerCapabilities {
        entries: vec![
            (CapabilityKind::StructuralParse, CapabilityLevel::Full),
            (CapabilityKind::SymbolExtraction, CapabilityLevel::Basic),
            (CapabilityKind::ImportExtraction, CapabilityLevel::None),
            (CapabilityKind::ReferenceExtraction, CapabilityLevel::None),
            (
                CapabilityKind::RelationshipResolution,
                CapabilityLevel::None,
            ),
        ],
    }
}

/// Map a `tags.scm` `@definition.<kind>` capture name to Attic's fixed
/// `SymbolKind` enum. Unrecognized kinds fall back to `SymbolKind::Variable`
/// (the most generic variant) rather than panicking or silently dropping the
/// symbol — every tag we can identify as a definition becomes *some* symbol.
fn map_symbol_kind(syntax_type: &str) -> SymbolKind {
    match syntax_type {
        "class" | "enum" | "object" => SymbolKind::Class,
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "interface" => SymbolKind::Interface,
        "module" => SymbolKind::Module,
        "macro" => SymbolKind::Macro,
        "type" => SymbolKind::TypeAlias,
        // "field"/"property"/"variable" fall through here too — no
        // dedicated SymbolKind exists for them, so they're intentionally
        // indistinguishable from the catch-all rather than listed
        // separately as if they were treated differently.
        _ => SymbolKind::Variable,
    }
}

fn span_from_points(range: std::ops::Range<tree_sitter::Point>) -> attic_core::SourceSpan {
    attic_core::SourceSpan::new(
        range.start.row as u32,
        range.start.column as u32,
        range.end.row as u32,
        range.end.column as u32,
    )
}

// ---------------------------------------------------------------------------
// Analyzer implementation
// ---------------------------------------------------------------------------

struct TagsAnalyzer {
    descriptor: AnalyzerDescriptor,
    language_tag: &'static str,
    config: TagsConfiguration,
}

impl Analyzer for TagsAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.descriptor
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        analyze_tags(
            self.language_tag,
            &self.config,
            &self.descriptor.name,
            input,
        )
    }
}

fn analyze_tags(
    language_tag: &'static str,
    config: &TagsConfiguration,
    analyzer_id: &str,
    input: AnalyzerInput,
) -> AnalyzerOutput {
    let started = Instant::now();
    let AnalyzerInput {
        file_occurrence_id,
        path,
        content,
        language_hint,
        file_type,
        size_bytes,
        is_partial_scan,
        cancellation_token,
        resource_budget,
    } = input;

    // Tags-based extraction runs FIRST, against a *borrow* of `content`'s
    // bytes (when available) — this avoids the full-file byte clone that used
    // to be required to hand an owned copy to both this pass and
    // GenericAnalyzer below (see `structural/mod.rs`'s `engine::run`, which
    // uses the analogous "borrow first, move later" shape via
    // `std::mem::replace`). `content` itself is moved into `generic_input`
    // completely unchanged once this borrow's scope ends.
    let mut tag_diagnostics: Vec<AnalyzerDiagnostic> = Vec::new();
    let mut tag_nodes: Vec<StructuralNodeSpec> = Vec::new();
    let mut tag_symbols: Vec<SymbolSpec> = Vec::new();
    let mut structurally_complete = true;
    // Set only for the two "no tags attempt was made at all" cases (streamed
    // LARGE file, or already-cancelled before extraction started) — mirrors
    // the original early-return paths, which reported `Lexical` regardless of
    // `out.symbols` (empty in both cases anyway).
    let mut skipped_entirely = false;

    match &content {
        AnalyzerContent::StreamingHandle(_) => {
            // LARGE (streamed) file: lexical-only, honestly marked — see module docs.
            tag_diagnostics.push(AnalyzerDiagnostic::warning(
                "STRUCTURAL_SKIPPED_LARGE_FILE",
                format!(
                    "{language_tag}: tags-based structural analysis is not attempted for \
                     streamed LARGE files; output is lexical-only via GenericAnalyzer."
                ),
            ));
            structurally_complete = false;
            skipped_entirely = true;
        }
        AnalyzerContent::FullBytes(bytes) | AnalyzerContent::RedactedBytes(bytes) => {
            if cancellation_token.is_cancelled() {
                tag_diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::CANCELLED,
                    "Cancelled before tags-based structural analysis started.",
                ));
                structurally_complete = false;
                skipped_entirely = true;
            } else {
                let entity_cap = TAGS_ENTITY_CAP.min(
                    usize::try_from(resource_budget.max_retrieval_units).unwrap_or(TAGS_ENTITY_CAP),
                );
                // Reuse one `TagsContext` (parser + cursor) per language per
                // thread rather than rebuilding it for every file: the
                // compiled `TagsConfiguration` is already cached per-language
                // via `OnceLock` (see `build_analyzer`), but `TagsContext`
                // wraps the actual tree-sitter parser, which is comparatively
                // expensive to construct. `thread_local!` (rather than a
                // `Mutex<TagsContext>` field) avoids lock contention when
                // multiple indexing threads analyze files concurrently, at
                // the cost of one parser per thread per language instead of
                // one globally — `generate_tags` resets all per-call parse
                // state itself (it parses `bytes` fresh with no previous
                // tree), so reusing the context across unrelated files of the
                // same language is safe.
                thread_local! {
                    static TAGS_CONTEXT_CACHE: std::cell::RefCell<std::collections::HashMap<&'static str, TagsContext>> =
                        std::cell::RefCell::new(std::collections::HashMap::new());
                }

                TAGS_CONTEXT_CACHE.with(|cache| {
                let mut cache = cache.borrow_mut();
                let ctx = cache
                    .entry(language_tag)
                    .or_insert_with(TagsContext::new);

                match ctx.generate_tags(config, bytes, None) {
                    Ok((tags_iter, has_error)) => {
                        if has_error {
                            tag_diagnostics.push(AnalyzerDiagnostic::warning(
                                "PARSE_ERROR",
                                format!(
                                    "{language_tag}: source contains syntax errors; structural output is partial"
                                ),
                            ));
                            structurally_complete = false;
                        }

                        let mut processed: u32 = 0;
                        'tags: for tag_result in tags_iter {
                            processed += 1;
                            if processed.is_multiple_of(CHECK_EVERY) {
                                if cancellation_token.is_cancelled() {
                                    tag_diagnostics.push(AnalyzerDiagnostic::warning(
                                        diagnostic_codes::CANCELLED,
                                        "Tags-based structural analysis cancelled mid-extraction; \
                                         output is PARTIAL.",
                                    ));
                                    structurally_complete = false;
                                    break 'tags;
                                }
                                if started.elapsed().as_millis() as u64 >= resource_budget.max_time_ms
                                {
                                    tag_diagnostics.push(AnalyzerDiagnostic::warning(
                                        diagnostic_codes::RESOURCE_EXHAUSTED,
                                        "Time budget exhausted during tags-based structural extraction; \
                                         output is PARTIAL.",
                                    ));
                                    structurally_complete = false;
                                    break 'tags;
                                }
                            }

                            let tag = match tag_result {
                                Ok(t) => t,
                                Err(_) => {
                                    structurally_complete = false;
                                    break 'tags;
                                }
                            };

                            // Only definitions become symbols — see module docs for why
                            // references are not represented anywhere in the output.
                            if !tag.is_definition {
                                continue;
                            }

                            if tag_symbols.len() >= entity_cap {
                                tag_diagnostics.push(AnalyzerDiagnostic::warning(
                                    diagnostic_codes::RESOURCE_EXHAUSTED,
                                    format!(
                                        "symbol cap ({entity_cap}) reached; further tags skipped — \
                                         output is PARTIAL"
                                    ),
                                ));
                                structurally_complete = false;
                                break 'tags;
                            }

                            let Some(name_bytes) = bytes.get(tag.name_range.clone()) else {
                                continue;
                            };
                            let name = String::from_utf8_lossy(name_bytes).into_owned();
                            if name.is_empty() {
                                continue;
                            }
                            let syntax_type =
                                config.syntax_type_name(tag.syntax_type_id).to_string();
                            let kind = map_symbol_kind(&syntax_type);
                            let span = span_from_points(tag.span.clone());
                            let identity_basis = format!("{language_tag}|{name}|{syntax_type}");
                            let structural_identity = super::structural_identity(&identity_basis);
                            let content_hash = bytes
                                .get(tag.range.clone())
                                .map(|b| blake3::hash(b).to_hex().to_string())
                                .unwrap_or_default();

                            let node_index = tag_nodes.len();
                            tag_nodes.push(StructuralNodeSpec {
                                node_type: syntax_type.to_ascii_uppercase(),
                                name: name.clone(),
                                span,
                                parent_index: None,
                                structural_identity,
                                content_hash,
                                metadata_json: None,
                            });
                            tag_symbols.push(SymbolSpec {
                                qualified_name: name.clone(),
                                short_name: name,
                                kind,
                                definition_span: span,
                                is_public: true,
                                disambiguator: None,
                                signature: None,
                                visibility: None,
                                is_definition: true,
                                node_index: Some(node_index),
                            });
                        }
                    }
                    Err(e) => {
                        tag_diagnostics.push(AnalyzerDiagnostic::error(
                            "TAGS_QUERY_FAILED",
                            format!("{language_tag}: failed to generate tags: {e}"),
                        ));
                        structurally_complete = false;
                    }
                }
                });
            }
        }
    }

    // Borrow of `content` ends here; move it unchanged into GenericAnalyzer's
    // input so retrieval-unit production is reused verbatim — no clone.
    let generic_input = AnalyzerInput {
        file_occurrence_id,
        path,
        content,
        language_hint,
        file_type,
        size_bytes,
        is_partial_scan,
        cancellation_token: cancellation_token.clone(),
        resource_budget: resource_budget.clone(),
    };
    let mut out = GenericAnalyzer::new().analyze(generic_input);
    out.analyzer_id = analyzer_id.to_string();
    out.analyzer_version = env!("CARGO_PKG_VERSION").to_string();

    out.diagnostics.extend(tag_diagnostics);
    out.structural_nodes.extend(tag_nodes);
    out.symbols.extend(tag_symbols);

    out.capability_used = if skipped_entirely {
        CapabilityKind::Lexical
    } else if out.symbols.is_empty() {
        CapabilityKind::StructuralParse
    } else {
        CapabilityKind::SymbolExtraction
    };
    out.structurally_complete = structurally_complete;
    out
}

// ---------------------------------------------------------------------------
// Registry wiring
// ---------------------------------------------------------------------------

fn build_analyzer(spec: TagsLanguageSpec) -> Option<(&'static str, Arc<dyn Analyzer>)> {
    let language: tree_sitter::Language = spec.grammar.into();
    match TagsConfiguration::new(language, spec.tags_query, spec.locals_query) {
        Ok(config) => {
            let descriptor = AnalyzerDescriptor {
                name: spec.analyzer_id.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: spec.description.to_string(),
                // Deliberately empty: these analyzers are registered ONLY via
                // `register_for_language`, never `register_specialized` (which
                // no-ops on an empty list) — see `default_registry`'s
                // exclusion-list comment.
                supported_file_types: vec![],
                capabilities: tier2_capabilities(),
            };
            let language_tag = spec.language_tag;
            let analyzer = Arc::new(TagsAnalyzer {
                descriptor,
                language_tag,
                config,
            }) as Arc<dyn Analyzer>;
            Some((language_tag, analyzer))
        }
        Err(e) => {
            tracing::error!(
                language = spec.language_tag,
                error = %e,
                "tags_generic: failed to compile tags query; this language will not get \
                 structural coverage (falls back to generic lexical analysis)"
            );
            None
        }
    }
}

fn build_tier2_analyzers() -> Vec<(&'static str, Arc<dyn Analyzer>)> {
    tier2_table()
        .into_iter()
        .filter_map(build_analyzer)
        .collect()
}

static TIER2: OnceLock<Vec<(&'static str, Arc<dyn Analyzer>)>> = OnceLock::new();

/// All tier-2 analyzers, `(language_tag, analyzer)` pairs, built once and
/// cheaply cloned (all `Arc`) on every call. Consumed by
/// [`super::default_registry`].
pub(crate) fn tier2_analyzers() -> Vec<(&'static str, Arc<dyn Analyzer>)> {
    TIER2.get_or_init(build_tier2_analyzers).clone()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves the hand-authored Dockerfile tags query actually works against
    /// the real grammar (kept from the original spike this module replaces).
    #[test]
    fn dockerfile_tags_query_extracts_stage_names() {
        let lang: tree_sitter::Language = tree_sitter_containerfile::LANGUAGE.into();
        let config = TagsConfiguration::new(lang, DOCKERFILE_TAGS_QUERY, "").unwrap();
        let mut ctx = TagsContext::new();
        let sample = b"FROM node:18 AS builder\nFROM builder AS runner\n";
        let (tags, _) = ctx.generate_tags(&config, sample, None).unwrap();
        let mut results = Vec::new();
        for tag in tags {
            let tag = tag.unwrap();
            let name = std::str::from_utf8(&sample[tag.name_range]).unwrap();
            let kind = config.syntax_type_name(tag.syntax_type_id);
            results.push((tag.is_definition, kind.to_string(), name.to_string()));
        }
        assert_eq!(
            results,
            vec![
                (false, "module".to_string(), "node:18".to_string()),
                (true, "module".to_string(), "builder".to_string()),
                (false, "module".to_string(), "builder".to_string()),
                (true, "module".to_string(), "runner".to_string()),
            ]
        );
    }

    /// Every table entry must compile its tags query against its grammar.
    /// A failure here means a query/grammar version mismatch that
    /// `build_analyzer`'s runtime fallback would otherwise silently swallow.
    #[test]
    fn all_tier2_queries_compile() {
        let table = tier2_table();
        let expected = table.len();
        let built = build_tier2_analyzers();
        assert_eq!(
            built.len(),
            expected,
            "every tier-2 tags query must compile against its grammar; \
             check tracing::error output for which one failed"
        );
    }

    #[test]
    fn tier2_analyzers_are_cached_and_cloneable() {
        let a = tier2_analyzers();
        let b = tier2_analyzers();
        assert_eq!(a.len(), b.len());
        assert!(!a.is_empty());
    }
}
