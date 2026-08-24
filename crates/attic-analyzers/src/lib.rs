//! `attic-analyzers` — Analyzer Foundation (Phase 1C)
//!
//! Provides the analyzer API, registry, and the mandatory `GenericAnalyzer`
//! that makes every decodable text file searchable without requiring a
//! language-specific parser.
//!
//! # Public API
//!
//! ```text
//! AnalyzerRegistry        — register and select analyzers
//! GenericAnalyzer         — mandatory language-agnostic fallback
//! Analyzer (trait)        — implement to add specialized analyzers
//! AnalyzerInput           — pre-processed content from Phase 1B
//! AnalyzerOutput          — structured analysis result
//! AnalyzerContent         — FullBytes / StreamingHandle / RedactedBytes
//! AnalyzerDescriptor      — stable analyzer identity
//! AnalyzerCapabilities    — declared capability set
//! CapabilityKind          — LEXICAL … SEMANTIC_RESOLUTION
//! CapabilityLevel         — NONE / BASIC / PARTIAL / FULL
//! ResourceBudget          — per-invocation resource limits
//! CancellationToken       — cooperative cancellation signal
//! AnalyzerDiagnostic      — structured diagnostic message
//! DiagnosticSeverity      — Info / Warning / Error
//! diagnostic_codes        — well-known code constants
//! ```
//!
//! # Security boundary
//!
//! Analyzers MUST consume content exclusively through `AnalyzerInput::content`.
//! They MUST NOT reopen `AnalyzerInput::path`.  The `AnalyzerContent` enum
//! encodes the Phase 1B security decision; `retrieval_text` in all output
//! units MUST NOT contain raw secret bytes.
#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod api;
pub mod cancellation;
pub mod generic;
pub mod registry;

// ── Flat re-exports ─────────────────────────────────────────────────────────

pub use api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, DiagnosticSeverity,
    ImportSpec, RelationshipSpec, ResourceBudget, RetrievalUnitSpec, StructuralNodeSpec,
    SymbolSpec, diagnostic_codes,
};
pub use cancellation::CancellationToken;
pub use generic::GenericAnalyzer;
pub use registry::AnalyzerRegistry;
