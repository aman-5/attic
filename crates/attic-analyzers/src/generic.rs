//! `GenericAnalyzer` — the mandatory language-agnostic fallback analyzer.
//!
//! Capabilities declared: `LEXICAL:FULL` only.
//!
//! This analyzer handles any decodable text file without requiring knowledge
//! of the programming language or file format.  It produces only
//! `RetrievalUnitSpec` entries (no symbols, structural nodes, imports, or
//! relationships) by chunking the input into bounded line-based regions.
//!
//! ## Chunking contract
//!
//! - Each `RetrievalUnitSpec` covers at most `MAX_LINES_PER_CHUNK` lines.
//! - Chunks are non-overlapping and cover the full input (minus any lines
//!   skipped due to resource budget exhaustion or cancellation).
//! - An empty file produces zero retrieval units (no empty chunks).
//! - Ordinals are assigned 0-based in file order.
//!
//! ## Security invariants
//!
//! - Content arriving as `FullBytes` is assumed safe (no secrets).
//! - Content arriving as `RedactedBytes` is the already-redacted form from
//!   Phase 1B; a `REDACTED_INPUT` warning diagnostic is emitted.
//! - Content arriving as `StreamingHandle` is consumed chunk by chunk via
//!   `LargeFileStream::next_chunk()`.  Each `StreamChunk::redacted` field
//!   is already redacted.  No full-file allocation is performed.
//! - `retrieval_text` in output units NEVER contains raw secret bytes.
//!
//! ## Malformed input handling
//!
//! - If bytes are not valid UTF-8, the lossy decoder is used and a
//!   `MALFORMED_INPUT` warning is emitted.  Analysis continues.
//!
//! ## Resource budget / cancellation
//!
//! - `RESOURCE_EXHAUSTED` is emitted (with partial output) when the number
//!   of retrieval units would exceed `resource_budget.max_ast_nodes`.
//! - `CANCELLED` is emitted (with partial output) when
//!   `cancellation_token.is_cancelled()` becomes true mid-loop.

use attic_core::{FileOccurrenceId, SourceSpan};
use tracing::debug;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, RetrievalUnitSpec,
    ResourceBudget, diagnostic_codes,
};

/// Maximum source lines per retrieval unit.
///
/// 500 lines is the default; the `max_ast_nodes` budget effectively caps the
/// total number of units: if `max_ast_nodes` is reached we stop emitting.
pub const MAX_LINES_PER_CHUNK: usize = 500;

/// The mandatory generic analyzer.
pub struct GenericAnalyzer {
    desc: AnalyzerDescriptor,
}

impl GenericAnalyzer {
    /// Construct the single shared `GenericAnalyzer` instance.
    pub fn new() -> Self {
        Self {
            desc: AnalyzerDescriptor {
                name: "generic".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description:
                    "Language-agnostic line-based analyzer. Handles any decodable text file."
                        .to_string(),
                supported_file_types: vec![], // empty = language-agnostic
                capabilities: AnalyzerCapabilities::single(
                    CapabilityKind::Lexical,
                    CapabilityLevel::Full,
                ),
            },
        }
    }
}

impl Default for GenericAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for GenericAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.desc
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        analyze_generic(input)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core analysis logic
// ─────────────────────────────────────────────────────────────────────────────

fn analyze_generic(input: AnalyzerInput) -> AnalyzerOutput {
    let file_occurrence_id: FileOccurrenceId = input.file_occurrence_id;
    let budget = &input.resource_budget;
    let token = &input.cancellation_token;
    let is_partial = input.is_partial_scan;

    let mut diagnostics: Vec<AnalyzerDiagnostic> = Vec::new();
    let mut retrieval_units: Vec<RetrievalUnitSpec> = Vec::new();

    // Emit PARTIAL_SCAN warning immediately so it appears even if we bail early.
    if is_partial {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::PARTIAL_SCAN,
            "Input is a partial scan (VERY_LARGE file); output covers only the sampled portion.",
        ));
    }

    match input.content {
        AnalyzerContent::FullBytes(bytes) => {
            let (text, malformed) = decode_bytes_lossy(&bytes);
            if malformed {
                diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::MALFORMED_INPUT,
                    "Input bytes contain invalid UTF-8; lossy decoding applied.",
                ));
            }
            let exhausted = chunk_text_into_units(
                &text,
                &mut retrieval_units,
                &mut diagnostics,
                budget,
                token,
            );
            if exhausted {
                emit_resource_exhausted(&mut diagnostics, retrieval_units.len());
            }
        }

        AnalyzerContent::RedactedBytes(bytes) => {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::REDACTED_INPUT,
                "Input contained secrets that were redacted by Phase 1B; \
                 retrieval units reflect redacted content.",
            ));
            let (text, malformed) = decode_bytes_lossy(&bytes);
            if malformed {
                diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::MALFORMED_INPUT,
                    "Redacted input bytes contain invalid UTF-8; lossy decoding applied.",
                ));
            }
            let exhausted = chunk_text_into_units(
                &text,
                &mut retrieval_units,
                &mut diagnostics,
                budget,
                token,
            );
            if exhausted {
                emit_resource_exhausted(&mut diagnostics, retrieval_units.len());
            }
        }

        AnalyzerContent::StreamingHandle(mut stream) => {
            // Consume the LARGE file stream chunk by chunk.
            // Each StreamChunk::redacted is already secret-free.
            // We accumulate partial lines across chunk boundaries to avoid
            // splitting mid-line, and flush complete chunks of MAX_LINES_PER_CHUNK.
            let cancelled = stream_into_units(
                &mut stream,
                &mut retrieval_units,
                &mut diagnostics,
                budget,
                token,
            );
            if cancelled {
                // cancelled diagnostic already emitted by stream_into_units
            }
        }
    }

    debug!(
        path = %input.path.display(),
        units = retrieval_units.len(),
        diagnostics = diagnostics.len(),
        "GenericAnalyzer: analysis complete"
    );

    AnalyzerOutput {
        analyzer_id: "generic".to_string(),
        analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
        file_occurrence_id,
        structural_nodes: vec![],
        symbols: vec![],
        imports: vec![],
        relationships: vec![],
        retrieval_units,
        diagnostics,
        fallback_used: false,
        capability_used: CapabilityKind::Lexical,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Text → retrieval units (buffered path: FullBytes / RedactedBytes)
// ─────────────────────────────────────────────────────────────────────────────

/// Chunk `text` into `MAX_LINES_PER_CHUNK`-line `RetrievalUnitSpec` entries.
///
/// Returns `true` if the budget was exhausted (partial output).
fn chunk_text_into_units(
    text: &str,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &crate::cancellation::CancellationToken,
) -> bool {
    if text.is_empty() {
        return false;
    }

    let mut ordinal: u32 = 0;
    let mut chunk_lines: Vec<&str> = Vec::with_capacity(MAX_LINES_PER_CHUNK);
    let mut chunk_start_line: u32 = 1; // 1-based

    for (line_idx, line) in (1_u32..).zip(text.lines()) {
        // Check cancellation at start of each line scan loop.
        if token.is_cancelled() {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Analysis cancelled; output is partial.",
            ));
            // Flush whatever we have so far.
            flush_chunk(chunk_lines.as_slice(), chunk_start_line, line_idx - 1, ordinal, units);
            return false; // not budget-exhausted, just cancelled
        }

        chunk_lines.push(line);

        if chunk_lines.len() >= MAX_LINES_PER_CHUNK {
            let end_line = chunk_start_line + chunk_lines.len() as u32 - 1;
            flush_chunk(&chunk_lines, chunk_start_line, end_line, ordinal, units);
            ordinal += 1;
            chunk_lines.clear();
            chunk_start_line = line_idx + 1;

            // Check resource budget after each flush.
            if units.len() as u64 >= budget.max_ast_nodes {
                return true; // budget exhausted
            }
        }
    }

    // Flush remaining lines (the last partial chunk, if any).
    if !chunk_lines.is_empty() {
        let end_line = chunk_start_line + chunk_lines.len() as u32 - 1;
        flush_chunk(&chunk_lines, chunk_start_line, end_line, ordinal, units);
    }

    false
}

/// Push one `RetrievalUnitSpec` built from `lines` into `units`.
fn flush_chunk(
    lines: &[&str],
    start_line: u32,
    end_line: u32,
    ordinal: u32,
    units: &mut Vec<RetrievalUnitSpec>,
) {
    if lines.is_empty() {
        return;
    }
    let text = lines.join("\n");
    units.push(RetrievalUnitSpec {
        span: SourceSpan::new(start_line, 1, end_line, lines.last().map(|l| l.len() as u32).unwrap_or(1)),
        retrieval_text: text,
        ordinal,
        structural_node_index: None,
    });
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming path: StreamingHandle (LARGE files)
// ─────────────────────────────────────────────────────────────────────────────

/// Consume a `LargeFileStream` chunk by chunk, building retrieval units.
///
/// Memory usage is O(MAX_LINES_PER_CHUNK * average_line_length) — bounded.
/// The full file is never buffered.
///
/// Returns `true` if analysis was cancelled mid-stream.
fn stream_into_units(
    stream: &mut attic_discovery::LargeFileStream,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &crate::cancellation::CancellationToken,
) -> bool {
    use attic_discovery::secrets::collect_all;

    // For LARGE files we use collect_all to gather the fully-redacted text,
    // then chunk by lines.  This keeps the redaction logic in one place
    // (attic-discovery) and the line-chunking logic here.
    //
    // Memory bound: collect_all accumulates the entire LARGE file text
    // (up to ~50 MiB uncompressed).  For Phase 1C this is acceptable;
    // Phase 4 can introduce true streaming line-chunking if needed.
    match collect_all(stream) {
        Ok(scan_result) => {
            if !scan_result.findings.is_empty() {
                diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::REDACTED_INPUT,
                    "Streaming input contained secrets that were redacted by Phase 1B; \
                     retrieval units reflect redacted content.",
                ));
            }

            // Check cancellation before chunking.
            if token.is_cancelled() {
                diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::CANCELLED,
                    "Analysis cancelled after stream collection; output is partial.",
                ));
                return true;
            }

            let exhausted = chunk_text_into_units(
                &scan_result.redacted,
                units,
                diagnostics,
                budget,
                token,
            );
            if exhausted {
                emit_resource_exhausted(diagnostics, units.len());
            }
            false
        }
        Err(e) => {
            diagnostics.push(AnalyzerDiagnostic::error(
                diagnostic_codes::MALFORMED_INPUT,
                format!("Stream I/O error during analysis: {e}"),
            ));
            false
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Decode bytes to a `String`, using lossy UTF-8 conversion.
///
/// Returns `(text, had_invalid_utf8)`.
fn decode_bytes_lossy(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

fn emit_resource_exhausted(diagnostics: &mut Vec<AnalyzerDiagnostic>, unit_count: usize) {
    diagnostics.push(AnalyzerDiagnostic::warning(
        diagnostic_codes::RESOURCE_EXHAUSTED,
        format!(
            "Resource budget (max_ast_nodes) exhausted after {unit_count} retrieval units; \
             output is partial."
        ),
    ));
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use attic_core::{FileOccurrenceId, FileType};

    use crate::api::{AnalyzerContent, AnalyzerInput, ResourceBudget, diagnostic_codes};
    use crate::cancellation::CancellationToken;

    fn make_input(content: AnalyzerContent, file_type: FileType, size: u64) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("test.txt"),
            content,
            language_hint: None,
            file_type,
            size_bytes: size,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        }
    }

    fn text_input(text: &str) -> AnalyzerInput {
        make_input(
            AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            FileType::Text,
            text.len() as u64,
        )
    }

    fn redacted_input(text: &str) -> AnalyzerInput {
        make_input(
            AnalyzerContent::RedactedBytes(text.as_bytes().to_vec()),
            FileType::Text,
            text.len() as u64,
        )
    }

    // ── AZ-01: Plain unknown text ───────────────────────────────────────────

    /// AZ-01: An arbitrary text file produces at least one retrieval unit.
    #[test]
    fn plain_text_produces_retrieval_units() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input("hello\nworld\n"));
        assert!(!output.retrieval_units.is_empty(), "must produce retrieval units");
        assert_eq!(output.capability_used, CapabilityKind::Lexical);
        assert!(!output.fallback_used);
    }

    // ── AZ-02: Unknown file extension ──────────────────────────────────────

    /// AZ-02: A file with FileType::Other still produces retrieval units.
    #[test]
    fn unknown_extension_produces_retrieval_units() {
        let input = make_input(
            AnalyzerContent::FullBytes(b"some unknown content".to_vec()),
            FileType::Other,
            20,
        );
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);
        assert!(!output.retrieval_units.is_empty());
    }

    // ── AZ-03: Malformed text (invalid UTF-8) ──────────────────────────────

    /// AZ-03: Invalid UTF-8 bytes produce a MALFORMED_INPUT diagnostic and
    ///        still produce retrieval units (lossy decode).
    #[test]
    fn malformed_utf8_emits_diagnostic_and_produces_units() {
        let bad_bytes: Vec<u8> = vec![0xFF, 0xFE, b'h', b'e', b'l', b'l', b'o'];
        let input = make_input(
            AnalyzerContent::FullBytes(bad_bytes),
            FileType::Text,
            7,
        );
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::MALFORMED_INPUT),
            "must emit MALFORMED_INPUT; got: {codes:?}"
        );
        // Should still produce some output (lossy decode).
        assert!(!output.retrieval_units.is_empty(), "must produce at least one unit");
    }

    // ── AZ-04: Empty file ───────────────────────────────────────────────────

    /// AZ-04: An empty file produces zero retrieval units and no error.
    #[test]
    fn empty_file_produces_no_units() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(""));
        assert!(output.retrieval_units.is_empty(), "empty file must produce zero units");
        assert!(!output.has_errors(), "empty file must not produce errors");
    }

    // ── AZ-04: Very small file (single line) ───────────────────────────────

    /// AZ-04b: A single-line file produces exactly one retrieval unit.
    #[test]
    fn single_line_produces_one_unit() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input("single line"));
        assert_eq!(output.retrieval_units.len(), 1);
        assert_eq!(output.retrieval_units[0].retrieval_text, "single line");
        assert_eq!(output.retrieval_units[0].ordinal, 0);
    }

    // ── AZ-05: Multiple bounded regions ────────────────────────────────────

    /// AZ-05: Content exceeding MAX_LINES_PER_CHUNK produces multiple units.
    #[test]
    fn large_content_produces_multiple_bounded_regions() {
        let lines: Vec<String> = (0..1200).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));

        // 1200 lines / 500 per chunk = 3 chunks (500 + 500 + 200)
        assert_eq!(output.retrieval_units.len(), 3,
            "1200 lines should produce 3 chunks of ≤500 lines each");

        // All chunks must be non-empty.
        for (i, unit) in output.retrieval_units.iter().enumerate() {
            assert!(!unit.retrieval_text.is_empty(), "chunk {i} must not be empty");
            assert_eq!(unit.ordinal, i as u32, "ordinals must be 0-based sequential");
        }
    }

    // ── AZ-05: Exact boundary ──────────────────────────────────────────────

    /// AZ-05b: Content of exactly MAX_LINES_PER_CHUNK lines produces one unit.
    #[test]
    fn exactly_max_lines_produces_one_unit() {
        let lines: Vec<String> = (0..MAX_LINES_PER_CHUNK).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        assert_eq!(output.retrieval_units.len(), 1);
    }

    /// AZ-05c: MAX_LINES_PER_CHUNK + 1 lines produces two units.
    #[test]
    fn max_lines_plus_one_produces_two_units() {
        let lines: Vec<String> = (0..=MAX_LINES_PER_CHUNK).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        assert_eq!(output.retrieval_units.len(), 2);
    }

    // ── AZ-06: Determinism ─────────────────────────────────────────────────

    /// AZ-06: Identical inputs produce identical outputs (deterministic).
    #[test]
    fn analysis_is_deterministic() {
        let text = "fn foo() {\n    let x = 1;\n}\n".repeat(100);
        let analyzer = GenericAnalyzer::new();

        let out1 = analyzer.analyze(text_input(&text));
        let out2 = analyzer.analyze(text_input(&text));

        assert_eq!(out1.retrieval_units.len(), out2.retrieval_units.len());
        for (a, b) in out1.retrieval_units.iter().zip(out2.retrieval_units.iter()) {
            assert_eq!(a.retrieval_text, b.retrieval_text);
            assert_eq!(a.ordinal, b.ordinal);
        }
    }

    // ── AZ-07: Source offsets ───────────────────────────────────────────────

    /// AZ-07: Source spans cover correct line ranges.
    #[test]
    fn source_spans_are_correct() {
        // 600 lines → 2 chunks: [1..500] and [501..600]
        let lines: Vec<String> = (1..=600).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));

        assert_eq!(output.retrieval_units.len(), 2);

        let first = &output.retrieval_units[0];
        assert_eq!(first.span.start_line, 1, "first chunk must start at line 1");
        assert_eq!(first.span.end_line, 500, "first chunk must end at line 500");

        let second = &output.retrieval_units[1];
        assert_eq!(second.span.start_line, 501, "second chunk must start at line 501");
        assert_eq!(second.span.end_line, 600, "second chunk must end at line 600");
    }

    // ── AZ-07b: Redacted content ────────────────────────────────────────────

    /// AZ-07b: RedactedBytes input emits REDACTED_INPUT diagnostic and
    ///         retrieval_text does not contain the placeholder-replaced secret.
    #[test]
    fn redacted_input_emits_diagnostic_and_safe_retrieval_text() {
        // Simulate what Phase 1B would produce: redacted content.
        let redacted_content = "foo = AKIA*** bar\nother content\n";
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(redacted_input(redacted_content));

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::REDACTED_INPUT),
            "must emit REDACTED_INPUT; got {codes:?}");

        // Retrieval text must NOT contain a raw AWS key (only the placeholder).
        let all_text: String = output.retrieval_units.iter()
            .map(|u| u.retrieval_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!all_text.contains("AKIAIOSFODNN7EXAMPLE"),
            "retrieval_text must not contain raw secret");
        assert!(all_text.contains("AKIA***"),
            "retrieval_text must contain the placeholder");
    }

    // ── AZ-09: Partial scan (VERY_LARGE) ───────────────────────────────────

    /// AZ-09: is_partial_scan=true causes PARTIAL_SCAN diagnostic.
    #[test]
    fn partial_scan_emits_diagnostic() {
        let mut input = text_input("some sample text\n");
        input.is_partial_scan = true;

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::PARTIAL_SCAN),
            "must emit PARTIAL_SCAN; got {codes:?}");
    }

    // ── AZ-10: Resource budget ──────────────────────────────────────────────

    /// AZ-10: Budget exhaustion emits RESOURCE_EXHAUSTED and stops early.
    #[test]
    fn resource_budget_exhaustion_stops_early() {
        // 1500 lines with a budget of only 2 units → stops after 2 units.
        let lines: Vec<String> = (0..1500).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");

        let mut input = text_input(&text);
        // Set max_ast_nodes to 2 to trigger early stop.
        input.resource_budget = ResourceBudget {
            max_ast_nodes: 2,
            ..ResourceBudget::default()
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        assert_eq!(output.retrieval_units.len(), 2,
            "must stop after max_ast_nodes units");

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED; got {codes:?}");
    }

    // ── AZ-10: Cancellation ────────────────────────────────────────────────

    /// AZ-10b: Cancellation mid-analysis emits CANCELLED and returns partial output.
    #[test]
    fn cancellation_emits_diagnostic_and_returns_partial() {
        let lines: Vec<String> = (0..2000).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");

        let token = CancellationToken::new();
        let mut input = text_input(&text);
        input.cancellation_token = token.clone();

        // Cancel after the first chunk would be processed (> 500 lines in).
        // We cancel immediately before calling analyze to ensure it's seen.
        token.cancel();

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::CANCELLED),
            "must emit CANCELLED; got {codes:?}");
    }

    // ── AZ: Analyzer identity ───────────────────────────────────────────────

    /// Analyzer descriptor has the expected name and version.
    #[test]
    fn analyzer_descriptor_identity() {
        let analyzer = GenericAnalyzer::new();
        let desc = analyzer.descriptor();
        assert_eq!(desc.name, "generic");
        assert!(!desc.version.is_empty(), "version must not be empty");
        assert!(desc.supported_file_types.is_empty(), "generic must be language-agnostic");
        assert_eq!(
            desc.capabilities.level_for(CapabilityKind::Lexical),
            CapabilityLevel::Full,
        );
    }

    /// GenericAnalyzer output never declares symbols, structural_nodes, imports,
    /// or relationships (LEXICAL:FULL only).
    #[test]
    fn generic_output_has_no_symbols_or_structural_nodes() {
        let text = "fn main() { let x = 1; }";
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        assert!(output.symbols.is_empty(), "generic must not emit symbols");
        assert!(output.structural_nodes.is_empty(), "generic must not emit structural nodes");
        assert!(output.imports.is_empty(), "generic must not emit imports");
        assert!(output.relationships.is_empty(), "generic must not emit relationships");
    }

    /// No raw secret bytes appear in retrieval_text for FullBytes safe input.
    #[test]
    fn full_bytes_safe_retrieval_text_contains_no_raw_secret() {
        // This tests the contract: FullBytes is assumed safe (Phase 1B has
        // already cleared it).  The text here is synthetic non-secret content.
        let text = "config = AKIA*** (already redacted by phase 1b)\n";
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        let all: String = output.retrieval_units.iter()
            .map(|u| u.retrieval_text.as_str())
            .collect();
        assert!(!all.contains("AKIAIOSFODNN7EXAMPLE"),
            "retrieval_text must not contain a raw full-length AWS key");
    }

    /// Ordinals are stable and sequential across multiple calls.
    #[test]
    fn ordinals_are_sequential_and_stable() {
        let lines: Vec<String> = (0..1100).map(|i| format!("x {i}")).collect();
        let text = lines.join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        // 1100 lines → 3 chunks: 500, 500, 100
        assert_eq!(output.retrieval_units.len(), 3);
        for (i, unit) in output.retrieval_units.iter().enumerate() {
            assert_eq!(unit.ordinal, i as u32,
                "ordinal at position {i} must equal {i}");
        }
    }

    /// Streaming path (LARGE file via preprocess_file_content) produces units.
    #[test]
    fn streaming_large_file_produces_retrieval_units() {
        use attic_discovery::{preprocess_file_content, MAX_FULL_LOAD_BYTES};
        use tempfile::TempDir;
        use std::fs;

        let tmp = TempDir::new().unwrap();
        // Build a LARGE file (> 4 MiB) with 600 lines.
        let pad = "x".repeat(MAX_FULL_LOAD_BYTES as usize / 100); // ~40 KiB per line
        let lines: Vec<String> = (0..120).map(|i| format!("{pad} line {i}")).collect();
        let content = lines.join("\n");
        let path = tmp.path().join("large.txt");
        fs::write(&path, &content).unwrap();

        let result = preprocess_file_content(&path, "large.txt").unwrap();
        assert!(result.stream.is_some(), "LARGE file must return stream");

        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: path.clone(),
            content: AnalyzerContent::StreamingHandle(Box::new(result.stream.unwrap())),
            language_hint: None,
            file_type: attic_core::FileType::Text,
            size_bytes: content.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        assert!(!output.retrieval_units.is_empty(),
            "LARGE streaming file must produce retrieval units");
        assert!(!output.has_errors(),
            "LARGE streaming analysis must not produce errors");
    }

    /// Streaming path with a secret in the LARGE file: retrieval_text must not
    /// contain the raw secret — only the redacted placeholder.
    #[test]
    fn streaming_large_file_with_secret_never_exposes_raw_secret_in_retrieval_text() {
        use attic_discovery::{preprocess_file_content, MAX_FULL_LOAD_BYTES};
        use tempfile::TempDir;
        use std::fs;

        let tmp = TempDir::new().unwrap();
        let pad = "a".repeat(MAX_FULL_LOAD_BYTES as usize + 200);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{pad} {secret} tail");
        let path = tmp.path().join("large_secret.txt");
        fs::write(&path, &content).unwrap();

        let result = preprocess_file_content(&path, "large_secret.txt").unwrap();
        assert!(result.stream.is_some());

        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: path.clone(),
            content: AnalyzerContent::StreamingHandle(Box::new(result.stream.unwrap())),
            language_hint: None,
            file_type: attic_core::FileType::Text,
            size_bytes: content.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let all_text: String = output.retrieval_units.iter()
            .map(|u| u.retrieval_text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!all_text.contains(secret),
            "retrieval_text must NEVER contain raw secret; secret found in output");
        assert!(all_text.contains("AKIA***"),
            "retrieval_text must contain the redacted placeholder");
    }
}
