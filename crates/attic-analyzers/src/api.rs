//! Analyzer API — trait and all input/output types per the approved contract.
//!
//! See `docs/contracts/analyzers.md` for the canonical definitions.
//! This module is the single authoritative Rust encoding of that contract.

use std::path::PathBuf;

use attic_core::{FileOccurrenceId, FileType, SourceSpan, SymbolKind};
use attic_discovery::LargeFileStream;
use serde::{Deserialize, Serialize};

use crate::cancellation::CancellationToken;

// ─────────────────────────────────────────────────────────────────────────────
// Capability model
// ─────────────────────────────────────────────────────────────────────────────

/// The *dimension* of analysis an analyzer performs.
///
/// These are **independent** dimensions, not an ordered hierarchy.
/// An analyzer may declare any combination of them at any level.
/// Registry selection must not assume that a higher numeric value implies
/// a superset of lower values — capabilities are orthogonal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapabilityKind {
    /// Line/token-based lexical decomposition.
    Lexical,
    /// Full parse tree / AST-backed structural decomposition.
    StructuralParse,
    /// Extraction of named symbol definitions (functions, classes, …).
    SymbolExtraction,
    /// Extraction of import/dependency edges.
    ImportExtraction,
    /// Cross-file reference edges within a single repo.
    ReferenceExtraction,
    /// Multi-file relationship resolution.
    RelationshipResolution,
    /// Build-system integration resolution.
    BuildResolution,
    /// Full semantic (type-checked) resolution.
    SemanticResolution,
}

/// How well the analyzer supports a given `CapabilityKind`.
///
/// Ordered: `None < Basic < Partial < Full`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CapabilityLevel {
    /// Capability not available for this analyzer + language combination.
    None = 0,
    /// Heuristic or approximate implementation.
    Basic = 1,
    /// Partial coverage with known gaps.
    Partial = 2,
    /// Complete, spec-conformant implementation.
    Full = 3,
}

/// The set of capabilities an analyzer declares.
///
/// At minimum one `CapabilityKind` must have a level > `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnalyzerCapabilities {
    /// For each declared capability: the kind and the level of support.
    ///
    /// Capabilities not listed are implicitly `CapabilityLevel::None`.
    pub entries: Vec<(CapabilityKind, CapabilityLevel)>,
}

impl AnalyzerCapabilities {
    /// Construct capabilities with a single entry.
    pub fn single(kind: CapabilityKind, level: CapabilityLevel) -> Self {
        Self {
            entries: vec![(kind, level)],
        }
    }

    /// Look up the level for a given kind. Returns `None` if not declared.
    pub fn level_for(&self, kind: CapabilityKind) -> CapabilityLevel {
        self.entries
            .iter()
            .find_map(|(k, l)| if *k == kind { Some(*l) } else { None })
            .unwrap_or(CapabilityLevel::None)
    }

    /// Return the best (highest) `CapabilityLevel` across all declared entries.
    ///
    /// This is the score used for registry selection when no specific capability
    /// dimension is requested.
    pub fn max_level(&self) -> CapabilityLevel {
        self.entries
            .iter()
            .map(|(_, l)| *l)
            .max()
            .unwrap_or(CapabilityLevel::None)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Analyzer identity
// ─────────────────────────────────────────────────────────────────────────────

/// Stable, unique identity of an analyzer implementation.
///
/// The `name` + `version` pair must be globally unique within a running
/// attic-analyzers instance. Stored in every `AnalyzerOutput` for provenance.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AnalyzerDescriptor {
    /// Short, kebab-case, human-readable name. E.g. `"generic"`, `"rust"`.
    pub name: String,
    /// Semver version string of this analyzer implementation.
    pub version: String,
    /// Human-readable description.
    pub description: String,
    /// The file types this analyzer claims to handle. Empty = language-agnostic.
    pub supported_file_types: Vec<FileType>,
    /// Declared capabilities.
    pub capabilities: AnalyzerCapabilities,
}

// ─────────────────────────────────────────────────────────────────────────────
// Resource budget
// ─────────────────────────────────────────────────────────────────────────────

/// Per-invocation resource limits. Analyzers MUST respect these.
///
/// When a limit is hit the analyzer MUST emit a `RESOURCE_EXHAUSTED` diagnostic
/// and return partial output rather than continuing indefinitely.
///
/// ## Field semantics per analyzer type
///
/// | Field | AST-materializing analyzers | GenericAnalyzer (lexical) |
/// |---|---|---|
/// | `max_memory_bytes` | max heap for AST/symbol tables | max cumulative `retrieval_text` bytes |
/// | `max_time_ms` | total wall-time budget | total wall-time budget |
/// | `max_ast_nodes` | max AST node count | not used (set to `u64::MAX`) |
/// | `max_retrieval_units` | advisory cap on output units | hard cap on `RetrievalUnit`s emitted |
/// | `max_recursion_depth` | max tree traversal depth | not used |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudget {
    /// Maximum memory the analyzer may allocate, in bytes.
    /// Default: 256 MiB.
    pub max_memory_bytes: u64,
    /// Maximum wall-time for the analysis, in milliseconds.
    /// Default: 30 000 ms (30 s).
    pub max_time_ms: u64,
    /// Maximum number of AST nodes that may be materialised.
    /// For `GenericAnalyzer` (no AST), this field is unused. Use
    /// `max_retrieval_units` to bound lexical output.
    /// Default: 1 000 000.
    pub max_ast_nodes: u64,
    /// Maximum number of `RetrievalUnit`s that may be emitted.
    /// Applies to all analyzers; enforced by `GenericAnalyzer` directly.
    /// Default: 10 000.
    pub max_retrieval_units: u64,
    /// Maximum recursion depth for tree traversal.
    /// Default: 500.
    pub max_recursion_depth: u32,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            max_memory_bytes: 256 * 1024 * 1024,
            max_time_ms: 30_000,
            max_ast_nodes: 1_000_000,
            max_retrieval_units: 10_000,
            max_recursion_depth: 500,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Analyzer input
// ─────────────────────────────────────────────────────────────────────────────

/// The content an analyzer receives, already pre-processed by Phase 1B.
///
/// Analyzers MUST NOT re-open the original file. All content is supplied
/// through this enum, which encodes the Phase 1B security decision.
///
/// Invariant (from secrets contract):
///   - `FullBytes`: safe, no secret bytes present.  Retrieval text may be stored.
///   - `RedactedBytes`: secrets were detected and replaced; retrieval text is safe.
///     Diagnostics must note redaction.
///   - `StreamingHandle`: LARGE file; the `LargeFileStream` has already applied
///     per-chunk redaction. Each `StreamChunk::redacted` is safe to index.
///     The stream MUST be consumed chunk by chunk; never accumulated fully.
pub enum AnalyzerContent {
    /// SMALL (≤4 MiB) or MEDIUM file — fully buffered, no secrets detected.
    FullBytes(Vec<u8>),
    /// LARGE (4–50 MiB) file — must be consumed chunk by chunk, O(1) memory.
    /// Each chunk yielded by the stream is already redacted.
    StreamingHandle(Box<LargeFileStream>),
    /// SMALL/MEDIUM file where secrets were detected and redacted by Phase 1B.
    RedactedBytes(Vec<u8>),
}

impl std::fmt::Debug for AnalyzerContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FullBytes(b) => write!(f, "FullBytes({} bytes)", b.len()),
            Self::StreamingHandle(_) => write!(f, "StreamingHandle(<stream>)"),
            Self::RedactedBytes(b) => write!(f, "RedactedBytes({} bytes)", b.len()),
        }
    }
}

/// Everything an analyzer needs for a single file analysis invocation.
pub struct AnalyzerInput {
    /// Identity of the file occurrence being analyzed (from Phase 1A storage).
    pub file_occurrence_id: FileOccurrenceId,
    /// Absolute path on the local filesystem. For logging/diagnostics only;
    /// do NOT re-open this path.
    pub path: PathBuf,
    /// Pre-processed content from Phase 1B.
    pub content: AnalyzerContent,
    /// Language hint from the discovery layer. May be `None` for unknown files.
    pub language_hint: Option<String>,
    /// File type classified by Phase 1B.
    pub file_type: FileType,
    /// Original file size in bytes (before any redaction).
    pub size_bytes: u64,
    /// Whether this is a partial scan (VERY_LARGE file or explicitly partial).
    pub is_partial_scan: bool,
    /// Cancellation signal; check frequently in loops.
    pub cancellation_token: CancellationToken,
    /// Per-invocation resource limits.
    pub resource_budget: ResourceBudget,
}

impl std::fmt::Debug for AnalyzerInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyzerInput")
            .field("file_occurrence_id", &self.file_occurrence_id)
            .field("path", &self.path)
            .field("content", &self.content)
            .field("language_hint", &self.language_hint)
            .field("file_type", &self.file_type)
            .field("size_bytes", &self.size_bytes)
            .field("is_partial_scan", &self.is_partial_scan)
            .finish_non_exhaustive()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Analyzer output types
// ─────────────────────────────────────────────────────────────────────────────

/// A structural node — a named region of a source file (function, class, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuralNodeSpec {
    /// Machine-readable node type tag. E.g. `"FUNCTION"`, `"CLASS"`, `"REGION"`.
    pub node_type: String,
    /// Human-readable name; empty for anonymous/region nodes.
    pub name: String,
    /// Source location. Byte-exact.
    pub span: SourceSpan,
    /// Optional parent node index within this file's output (0-based).
    pub parent_index: Option<usize>,
    /// Rename-stable identity hash (BLAKE3 of a path-independent basis).
    /// Persists as `core_structural_nodes.structural_identity`.
    pub structural_identity: String,
    /// BLAKE3 hex of the node's source bytes *as delivered* (post-redaction).
    pub content_hash: String,
    /// Analyzer-specific structured metadata; never contains secret content.
    pub metadata_json: Option<String>,
}

/// A symbol definition found in this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolSpec {
    /// Qualified name (language-specific format).
    pub qualified_name: String,
    /// Unqualified (short) name.
    pub short_name: String,
    /// Kind of symbol.
    pub kind: SymbolKind,
    /// Where it is defined.
    pub definition_span: SourceSpan,
    /// Whether the symbol is publicly visible outside its module.
    pub is_public: bool,
    /// Disambiguator when `(language, qualified_name, kind)` is ambiguous
    /// (e.g. Java/TypeScript overloads): `"overload:N"` ordered by span.
    pub disambiguator: Option<String>,
    /// Language-specific signature text, when extractable.
    pub signature: Option<String>,
    /// Raw visibility modifier (`public`/`private`/`package`…), when present.
    pub visibility: Option<String>,
    /// `false` for pure signatures (abstract/interface members) that declare
    /// API surface without a body.
    pub is_definition: bool,
    /// Index of the anchoring structural node within this output.
    pub node_index: Option<usize>,
}

/// An import or dependency edge found in this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSpec {
    /// The raw import specifier string as written in source.
    pub raw_specifier: String,
    /// Resolved file path, if deterministically resolvable without build system.
    pub resolved_path: Option<PathBuf>,
    /// Source location of the import statement.
    pub span: SourceSpan,
    /// Language-specific import form. Well-known values:
    /// `IMPORT` (Java/Go/plain), `FROM` (Python from-import),
    /// `REQUIRE` (CommonJS), `DYNAMIC` (`import()`), `EXPORT_FROM`,
    /// `IMPORT_TYPE` (TypeScript type-only import).
    pub import_kind: String,
}

/// How a relationship edge was established (contract: `resolution` column).
///
/// This is an *honesty* axis: an edge must never claim a higher level than
/// the evidence that produced it. A syntactic/name match must stay
/// [`ResolutionLevel::Syntactic`] even when it looks plausible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ResolutionLevel {
    /// Derived purely from source syntax; target not confirmed to exist.
    Syntactic,
    /// Target confirmed against package/module layout (file exists in repo).
    PackageResolved,
    /// Target confirmed against a known symbol definition in the repo.
    SymbolResolved,
    /// Target confirmed through build-system metadata (go.mod, pom.xml, …).
    BuildResolved,
    /// Target confirmed through framework-specific knowledge.
    FrameworkResolved,
    /// Heuristic inference only; lowest trust.
    Inferred,
}

impl ResolutionLevel {
    /// Canonical DB token (`core_relationships.resolution` enum).
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Syntactic => "SYNTACTIC",
            Self::PackageResolved => "PACKAGE_RESOLVED",
            Self::SymbolResolved => "SYMBOL_RESOLVED",
            Self::BuildResolved => "BUILD_RESOLVED",
            Self::FrameworkResolved => "FRAMEWORK_RESOLVED",
            Self::Inferred => "INFERRED",
        }
    }
}

/// A cross-file relationship edge (e.g. implementation of a trait/interface).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationshipSpec {
    /// Relationship type tag. E.g. `"IMPLEMENTS"`, `"EXTENDS"`, `"USES"`.
    pub relationship_type: String,
    /// The target qualified name.
    pub target_qualified_name: String,
    /// Source location of the relationship.
    pub span: SourceSpan,
    /// How this edge was established. Never infer beyond actual evidence.
    pub resolution: ResolutionLevel,
    /// Confidence in `[0.0, 1.0]`. Syntactic edges typically ≤ 0.6;
    /// resolved edges carry the resolver's confidence.
    pub confidence: f64,
    /// When the edge originates from a specific symbol defined in this same
    /// output (method call inside a class member, class heritage, …): index
    /// into this file's `symbols` vec. `None` = file-scoped edge (imports).
    pub source_symbol_index: Option<usize>,
}

impl RelationshipSpec {
    /// Construct a syntax-only edge at the conventional syntactic confidence.
    pub fn syntactic(
        relationship_type: impl Into<String>,
        target_qualified_name: impl Into<String>,
        span: SourceSpan,
    ) -> Self {
        Self {
            relationship_type: relationship_type.into(),
            target_qualified_name: target_qualified_name.into(),
            span,
            resolution: ResolutionLevel::Syntactic,
            confidence: 0.5,
            source_symbol_index: None,
        }
    }
}

/// A single chunk of text that can be stored as an independently retrievable
/// unit (e.g. one page of a document, one function body, one 500-line region).
///
/// Security invariant: `retrieval_text` MUST NOT contain raw secret bytes.
/// If the source was `RedactedBytes` or `StreamingHandle`, only the
/// already-redacted text may appear here.
///
/// ## Span semantics
///
/// `span.start_line` and `span.end_line` are **1-based** line numbers in the
/// source file as delivered (post-redaction for `RedactedBytes`; the emitted
/// chunk bytes for `StreamingHandle`).
///
/// `span.start_byte` and `span.end_byte` are byte offsets within the content
/// as delivered to this analyzer (i.e. within `retrieval_text`, not the
/// original pre-redaction file when redaction changed byte lengths).
///
/// For `FullBytes` inputs with no redaction the byte offsets correspond exactly
/// to the original file positions.  For `RedactedBytes` and `StreamingHandle`
/// inputs they correspond to positions in the redacted representation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalUnitSpec {
    /// Span within the content as delivered to this analyzer.
    pub span: SourceSpan,
    /// Safe-to-store text for full-text indexing. Never contains raw secrets.
    pub retrieval_text: String,
    /// Ordinal index of this unit within the file (0-based, stable ordering).
    pub ordinal: u32,
    /// Optional structural node index this unit is associated with.
    pub structural_node_index: Option<usize>,
}

/// Severity of a diagnostic emitted by an analyzer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiagnosticSeverity {
    /// Informational; analysis succeeded fully.
    Info,
    /// Analysis succeeded but with caveats (e.g. partial scan, redacted input).
    Warning,
    /// Analysis failed partially; output may be incomplete.
    Error,
}

/// A diagnostic message emitted during analysis.
///
/// Well-known `code` values (consumers may match on these):
///
/// | Code                | Meaning |
/// |---------------------|---------|
/// | `FALLBACK_USED`     | Specialized analyzer failed; output is from GenericAnalyzer |
/// | `PANIC_CAUGHT`      | Specialized analyzer panicked; recovered via fallback |
/// | `PARTIAL_SCAN`      | Input was a VERY_LARGE partial scan; output is incomplete |
/// | `RESOURCE_EXHAUSTED`| A resource budget limit was hit; output truncated |
/// | `REDACTED_INPUT`    | Input contained redacted secrets; retrieval units reflect redaction |
/// | `CANCELLED`         | Cancellation was signalled; output is partial |
/// | `MALFORMED_INPUT`   | Input bytes were not valid UTF-8 or otherwise malformed |
/// | `UNSTABLE_CAPTURE`  | Source revision was unstable during capture |
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyzerDiagnostic {
    /// Machine-readable diagnostic code (see table above).
    pub code: String,
    /// Human-readable message. Must not contain sensitive data.
    pub message: String,
    /// Severity.
    pub severity: DiagnosticSeverity,
    /// Optional source span this diagnostic relates to.
    pub span: Option<SourceSpan>,
}

impl AnalyzerDiagnostic {
    /// Construct a warning-level diagnostic with the given code and message.
    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity: DiagnosticSeverity::Warning,
            span: None,
        }
    }

    /// Construct an error-level diagnostic.
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity: DiagnosticSeverity::Error,
            span: None,
        }
    }

    /// Construct an info-level diagnostic.
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            severity: DiagnosticSeverity::Info,
            span: None,
        }
    }
}

/// Well-known diagnostic codes. Use these constants instead of raw strings.
pub mod diagnostic_codes {
    pub const FALLBACK_USED: &str = "FALLBACK_USED";
    pub const PANIC_CAUGHT: &str = "PANIC_CAUGHT";
    pub const PARTIAL_SCAN: &str = "PARTIAL_SCAN";
    pub const RESOURCE_EXHAUSTED: &str = "RESOURCE_EXHAUSTED";
    pub const REDACTED_INPUT: &str = "REDACTED_INPUT";
    pub const CANCELLED: &str = "CANCELLED";
    pub const MALFORMED_INPUT: &str = "MALFORMED_INPUT";
    pub const UNSTABLE_CAPTURE: &str = "UNSTABLE_CAPTURE";
}

/// The complete output of a single analyzer invocation.
#[derive(Debug)]
pub struct AnalyzerOutput {
    /// Which analyzer produced this output.
    pub analyzer_id: String,
    /// Version of the analyzer.
    pub analyzer_version: String,
    /// The file occurrence this output covers.
    pub file_occurrence_id: FileOccurrenceId,
    /// Structural nodes found (functions, classes, regions, …).
    pub structural_nodes: Vec<StructuralNodeSpec>,
    /// Symbol definitions found.
    pub symbols: Vec<SymbolSpec>,
    /// Import/dependency edges found.
    pub imports: Vec<ImportSpec>,
    /// Cross-file relationship edges.
    pub relationships: Vec<RelationshipSpec>,
    /// Retrieval units (independently indexable chunks).
    pub retrieval_units: Vec<RetrievalUnitSpec>,
    /// Diagnostics emitted during this analysis.
    pub diagnostics: Vec<AnalyzerDiagnostic>,
    /// `true` if a specialized analyzer was attempted but failed and the output
    /// was produced by `GenericAnalyzer` as fallback.
    pub fallback_used: bool,
    /// The highest `CapabilityKind` that was successfully applied.
    pub capability_used: CapabilityKind,
}

impl AnalyzerOutput {
    /// Convenience: does this output carry any error-severity diagnostic?
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Analyzer trait
// ─────────────────────────────────────────────────────────────────────────────

/// The core trait every analyzer must implement.
///
/// Implementations must be `Send + Sync` so they can be shared across threads
/// in the registry.
///
/// Contract invariants (from `docs/contracts/analyzers.md`):
/// 1. Must check `input.cancellation_token.is_cancelled()` at the start of
///    every O(n) loop and return partial output with a `CANCELLED` diagnostic.
/// 2. Must respect `input.resource_budget` and emit `RESOURCE_EXHAUSTED`
///    when a limit is approached.  `max_retrieval_units` bounds lexical output.
///    `max_memory_bytes` bounds cumulative retrieval_text bytes.
///    `max_time_ms` bounds wall-clock time.
/// 3. `AnalyzerOutput::retrieval_units[*].retrieval_text` MUST NOT contain
///    raw secret bytes.
/// 4. `analyze()` must never panic. If a parse or internal error occurs, emit
///    an error diagnostic and return partial (or empty) output.
/// 5. Must never re-open `input.path`. Content arrives only through
///    `input.content`.
pub trait Analyzer: Send + Sync {
    /// Return stable metadata about this analyzer.
    fn descriptor(&self) -> &AnalyzerDescriptor;

    /// Analyze the given input and return structured output.
    ///
    /// This is a synchronous, blocking call. Phase 4 will introduce async
    /// variants; the synchronous contract is preserved for Phase 1C.
    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput;
}
