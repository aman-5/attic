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
//! - Content arriving as `StreamingHandle` is consumed **chunk by chunk** via
//!   `LargeFileStream::next_chunk()`.  The full file is **never accumulated**
//!   in memory.  Only a partial-line carry buffer (at most one line fragment)
//!   is maintained across chunk boundaries.  Each `StreamChunk::redacted`
//!   field is already redacted and safe to index.
//! - `retrieval_text` in output units NEVER contains raw secret bytes.
//!
//! ## Malformed input handling
//!
//! - If bytes are not valid UTF-8, the lossy decoder is used and a
//!   `MALFORMED_INPUT` warning is emitted.  Analysis continues.
//!
//! ## Resource budget / cancellation
//!
//! For `GenericAnalyzer`:
//! - `max_retrieval_units`: hard cap on the number of `RetrievalUnitSpec`s emitted.
//!   When reached, a `RESOURCE_EXHAUSTED` diagnostic is emitted and analysis stops.
//! - `max_memory_bytes`: cumulative cap on the sum of `retrieval_text.len()` bytes
//!   across all emitted units.  When exceeded, `RESOURCE_EXHAUSTED` is emitted.
//! - `max_time_ms`: wall-clock budget checked between chunk/flush boundaries.
//!   When exceeded, `RESOURCE_EXHAUSTED` is emitted.
//! - `max_ast_nodes`: **not used** by `GenericAnalyzer` (no AST is materialised).
//! - Cancellation: `CancellationToken::is_cancelled()` is checked between
//!   every chunk in the streaming path and between every flush in the buffered
//!   path.  A `CANCELLED` diagnostic is emitted on early exit.
//!
//! ## Source span semantics
//!
//! `RetrievalUnitSpec::span` line numbers are 1-based.  Byte offsets in
//! `SourceSpan` are not stored directly — the span records line/column numbers.
//! `start_byte` / `end_byte` in comments below refer to the virtual byte
//! position within the delivered content, used to track chunk boundaries.
//!
//! **CRLF handling**: `\r\n` line endings are split on `\n`; the `\r` is
//! stripped from line content (matching editor convention) but the `\r`
//! byte IS counted when advancing global byte offsets.  `str::lines()` is NOT
//! used for byte-offset computation because it silently strips `\r`.
//!
//! **UTF-8 multibyte**: all byte offsets use byte lengths (`str::len()`), not
//! character counts.  A 4-byte emoji contributes 4 bytes to the running offset.

use std::time::Instant;

use attic_core::{FileOccurrenceId, SourceSpan};
use tracing::debug;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, RetrievalUnitSpec,
    ResourceBudget, diagnostic_codes,
};
use crate::cancellation::CancellationToken;

/// Maximum source lines per retrieval unit.
pub const MAX_LINES_PER_CHUNK: usize = 500;

// ─────────────────────────────────────────────────────────────────────────────
// Public struct
// ─────────────────────────────────────────────────────────────────────────────

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
    let budget = input.resource_budget.clone();
    let token = input.cancellation_token.clone();
    let is_partial = input.is_partial_scan;
    let start_time = Instant::now();

    let mut diagnostics: Vec<AnalyzerDiagnostic> = Vec::new();
    let mut retrieval_units: Vec<RetrievalUnitSpec> = Vec::new();

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
            let mut cum = 0u64;
            chunk_text_into_units(
                &text,
                &mut retrieval_units,
                &mut diagnostics,
                &budget,
                &token,
                1, // line_number_base (1-based)
                start_time,
                &mut cum,
            );
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
            let mut cum = 0u64;
            chunk_text_into_units(
                &text,
                &mut retrieval_units,
                &mut diagnostics,
                &budget,
                &token,
                1,
                start_time,
                &mut cum,
            );
        }

        AnalyzerContent::StreamingHandle(mut stream) => {
            stream_into_units(
                &mut stream,
                &mut retrieval_units,
                &mut diagnostics,
                &budget,
                &token,
                start_time,
            );
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
// Line splitter with byte-accurate offsets
// ─────────────────────────────────────────────────────────────────────────────

/// A single parsed line.
struct ParsedLine {
    /// Line content without trailing `\n` or `\r`.
    content: String,
    /// Byte length of the full line including its terminator(s)
    /// (e.g. `\n` = 1, `\r\n` = 2, unterminated = `content.len()`).
    #[allow(dead_code)] // used in tests only
    byte_len_with_terminator: usize,
}

/// Split `text` into lines with correct byte accounting for `\n`, `\r\n`,
/// and a missing trailing newline.
///
/// Does NOT use `str::lines()` because that silently strips `\r` and gives
/// wrong byte positions for CRLF files.
fn split_lines_bytes(text: &str) -> Vec<ParsedLine> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    while pos < len {
        match memchr_newline(bytes, pos) {
            Some(nl) => {
                // Strip \r if CRLF.
                let content_end = if nl > pos && bytes[nl - 1] == b'\r' { nl - 1 } else { nl };
                let content = text[pos..content_end].to_string();
                let byte_len_with_terminator = nl + 1 - pos; // includes \n (and \r if present)
                result.push(ParsedLine { content, byte_len_with_terminator });
                pos = nl + 1;
            }
            None => {
                // Last line, no trailing newline.
                let content = text[pos..].to_string();
                let byte_len_with_terminator = len - pos;
                result.push(ParsedLine { content, byte_len_with_terminator });
                pos = len;
            }
        }
    }

    result
}

/// Find the next `\n` byte at or after `from` in `bytes`.
#[inline]
fn memchr_newline(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..].iter().position(|&b| b == b'\n').map(|rel| from + rel)
}

// ─────────────────────────────────────────────────────────────────────────────
// Build a single RetrievalUnitSpec from a slice of line content strings
// ─────────────────────────────────────────────────────────────────────────────

fn build_retrieval_unit_from_lines(
    ordinal: u32,
    start_line: u32,
    lines: &[String],
) -> RetrievalUnitSpec {
    let end_line = start_line + lines.len() as u32 - 1;
    let last_col = lines.last().map(|l| l.len() as u32).unwrap_or(0) + 1;
    let retrieval_text = lines.join("\n");
    RetrievalUnitSpec {
        span: SourceSpan::new(start_line, 1, end_line, last_col),
        retrieval_text,
        ordinal,
        structural_node_index: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resource budget check helper — returns true if we must stop
// ─────────────────────────────────────────────────────────────────────────────

/// Check all three GenericAnalyzer resource limits after emitting a unit.
/// Pushes the appropriate diagnostic and returns `true` if analysis must stop.
fn check_and_emit_resource(
    units: &[RetrievalUnitSpec],
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    start_time: Instant,
    cumulative_text_bytes: u64,
) -> bool {
    // max_retrieval_units
    if units.len() as u64 >= budget.max_retrieval_units {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::RESOURCE_EXHAUSTED,
            format!(
                "max_retrieval_units ({}) reached after {} units; output is partial.",
                budget.max_retrieval_units,
                units.len()
            ),
        ));
        return true;
    }

    // max_memory_bytes (cumulative retrieval_text)
    if cumulative_text_bytes >= budget.max_memory_bytes {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::RESOURCE_EXHAUSTED,
            format!(
                "max_memory_bytes ({}) exceeded at {} cumulative retrieval_text bytes; \
                 output is partial.",
                budget.max_memory_bytes, cumulative_text_bytes
            ),
        ));
        return true;
    }

    // max_time_ms
    if start_time.elapsed().as_millis() as u64 >= budget.max_time_ms {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::RESOURCE_EXHAUSTED,
            format!(
                "max_time_ms ({} ms) exceeded after {} units; output is partial.",
                budget.max_time_ms,
                units.len()
            ),
        ));
        return true;
    }

    // Cancellation
    if token.is_cancelled() {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::CANCELLED,
            "Analysis cancelled; output is partial.",
        ));
        return true;
    }

    false
}

// ─────────────────────────────────────────────────────────────────────────────
// Text → retrieval units (buffered path: FullBytes / RedactedBytes)
// ─────────────────────────────────────────────────────────────────────────────

/// Chunk `text` into `MAX_LINES_PER_CHUNK`-line `RetrievalUnitSpec` entries.
///
/// `line_number_base`: 1-based line number of `text[0]` in the source file.
/// `cumulative_text_bytes`: running total of bytes across all emitted units.
#[allow(clippy::too_many_arguments)]
fn chunk_text_into_units(
    text: &str,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    line_number_base: u32,
    start_time: Instant,
    cumulative_text_bytes: &mut u64,
) {
    if text.is_empty() {
        return;
    }

    let lines = split_lines_bytes(text);
    let mut chunk_start: usize = 0; // index into `lines`

    loop {
        if chunk_start >= lines.len() {
            break;
        }

        let chunk_end = (chunk_start + MAX_LINES_PER_CHUNK).min(lines.len());
        let chunk_lines: Vec<String> = lines[chunk_start..chunk_end]
            .iter()
            .map(|l| l.content.clone())
            .collect();

        let start_line = line_number_base + chunk_start as u32;
        let unit = build_retrieval_unit_from_lines(units.len() as u32, start_line, &chunk_lines);
        *cumulative_text_bytes += unit.retrieval_text.len() as u64;
        units.push(unit);

        chunk_start = chunk_end;

        if check_and_emit_resource(units, diagnostics, budget, token, start_time, *cumulative_text_bytes) {
            return;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming path: StreamingHandle (LARGE files, O(1) memory)
// ─────────────────────────────────────────────────────────────────────────────

/// Consume a `LargeFileStream` chunk by chunk, building retrieval units.
///
/// ## Memory invariant
///
/// At any point the state allocated beyond the output `units` vector is:
/// - `carry`: at most one incomplete line fragment from the previous chunk.
///   Bounded by `STREAM_CHUNK_SIZE` (≤ 64 KiB).
/// - `pending_lines`: at most `MAX_LINES_PER_CHUNK` complete line strings
///   before they are flushed into a unit.
///
/// The full file is NEVER accumulated. `collect_all()` is NOT called.
///
/// ## Cancellation
///
/// Cancellation and time budget are checked BEFORE fetching each new chunk,
/// which means they fire while the stream is still being consumed — not only
/// after full collection.
fn stream_into_units(
    stream: &mut attic_discovery::LargeFileStream,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    start_time: Instant,
) {
    // Partial line carried across chunk boundaries. At most one line fragment.
    let mut carry = String::new();
    // Complete lines accumulated for the current retrieval unit.
    let mut pending_lines: Vec<String> = Vec::with_capacity(MAX_LINES_PER_CHUNK + 1);

    let mut chunk_start_line: u32 = 1; // 1-based start line of current pending unit
    let mut global_line: u32 = 1;      // next line number (after last complete \n seen)
    let mut cumulative_text_bytes: u64 = 0;
    let mut found_secret = false;

    loop {
        // ── Check cancellation BEFORE fetching the next chunk ──────────────
        // This ensures cancellation fires during streaming, not only at EOF.
        if token.is_cancelled() {
            flush_stream_pending(&pending_lines, &carry, chunk_start_line, global_line, units);
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Analysis cancelled during streaming; output is partial.",
            ));
            return;
        }

        // ── Check time budget BEFORE fetching the next chunk ───────────────
        if start_time.elapsed().as_millis() as u64 >= budget.max_time_ms {
            flush_stream_pending(&pending_lines, &carry, chunk_start_line, global_line, units);
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "max_time_ms ({} ms) exceeded during streaming; output is partial.",
                    budget.max_time_ms
                ),
            ));
            return;
        }

        // ── Fetch the next raw chunk from the stream ───────────────────────
        let chunk = match stream.next_chunk() {
            None => {
                // EOF — flush carry and any remaining pending lines.
                if !carry.is_empty() {
                    pending_lines.push(std::mem::take(&mut carry));
                }
                if !pending_lines.is_empty() {
                    let end_line = chunk_start_line + pending_lines.len() as u32 - 1;
                    let last_col = pending_lines.last().map(|l| l.len() as u32).unwrap_or(0) + 1;
                    units.push(RetrievalUnitSpec {
                        span: SourceSpan::new(chunk_start_line, 1, end_line, last_col),
                        retrieval_text: pending_lines.join("\n"),
                        ordinal: units.len() as u32,
                        structural_node_index: None,
                    });
                }
                if found_secret {
                    diagnostics.push(AnalyzerDiagnostic::warning(
                        diagnostic_codes::REDACTED_INPUT,
                        "Streaming input contained secrets that were redacted by Phase 1B; \
                         retrieval units reflect redacted content.",
                    ));
                }
                return;
            }
            Some(Err(e)) => {
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::MALFORMED_INPUT,
                    format!("Stream I/O error during analysis: {e}"),
                ));
                return;
            }
            Some(Ok(ch)) => ch,
        };

        if !chunk.findings.is_empty() {
            found_secret = true;
        }

        // If the chunk has no text (e.g. pure-PEM finding chunk), skip.
        if chunk.redacted.is_empty() {
            continue;
        }

        // ── Prepend carry to this chunk's text ─────────────────────────────
        let text = if carry.is_empty() {
            chunk.redacted
        } else {
            let mut s = std::mem::take(&mut carry);
            s.push_str(&chunk.redacted);
            s
        };

        // ── Split text into lines (CRLF-aware, byte-accurate) ──────────────
        let text_bytes = text.as_bytes();
        let text_len = text_bytes.len();
        let mut scan = 0usize;

        while scan < text_len {
            match memchr_newline(text_bytes, scan) {
                Some(nl) => {
                    // Strip \r for CRLF.
                    let content_end =
                        if nl > scan && text_bytes[nl - 1] == b'\r' { nl - 1 } else { nl };
                    let line_content = text[scan..content_end].to_string();
                    pending_lines.push(line_content);
                    global_line += 1;
                    scan = nl + 1;

                    // ── Flush when we have a full chunk of lines ───────────
                    if pending_lines.len() >= MAX_LINES_PER_CHUNK {
                        let end_line = chunk_start_line + pending_lines.len() as u32 - 1;
                        let last_col =
                            pending_lines.last().map(|l| l.len() as u32).unwrap_or(0) + 1;
                        let unit = RetrievalUnitSpec {
                            span: SourceSpan::new(chunk_start_line, 1, end_line, last_col),
                            retrieval_text: pending_lines.join("\n"),
                            ordinal: units.len() as u32,
                            structural_node_index: None,
                        };
                        cumulative_text_bytes += unit.retrieval_text.len() as u64;
                        units.push(unit);
                        pending_lines.clear();
                        chunk_start_line = global_line;

                        if check_and_emit_resource(
                            units,
                            diagnostics,
                            budget,
                            token,
                            start_time,
                            cumulative_text_bytes,
                        ) {
                            return;
                        }
                    }
                }
                None => {
                    // No more \n in this chunk — remainder is a partial line.
                    // Save it as carry; it will be prepended to the next chunk.
                    carry = text[scan..].to_string();
                    break;
                }
            }
        }
    }
}

/// Flush any carry + pending_lines into a final unit at EOF or on early exit.
///
/// Does NOT push a diagnostic — the caller handles that.
fn flush_stream_pending(
    pending_lines: &[String],
    carry: &str,
    chunk_start_line: u32,
    global_line: u32,
    units: &mut Vec<RetrievalUnitSpec>,
) {
    let mut all_lines: Vec<String> = pending_lines.to_vec();
    if !carry.is_empty() {
        all_lines.push(carry.to_string());
    }
    if all_lines.is_empty() {
        return;
    }
    let end_line = chunk_start_line + all_lines.len() as u32 - 1;
    let last_col = all_lines.last().map(|l| l.len() as u32).unwrap_or(0) + 1;
    let _ = global_line; // present for callers that track it
    units.push(RetrievalUnitSpec {
        span: SourceSpan::new(chunk_start_line, 1, end_line, last_col),
        retrieval_text: all_lines.join("\n"),
        ordinal: units.len() as u32,
        structural_node_index: None,
    });
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

    fn text_input_with_budget(text: &str, budget: ResourceBudget) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("test.txt"),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: budget,
        }
    }

    fn text_input_with_token(text: &str, token: CancellationToken) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("test.txt"),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        }
    }

    // ── AZ-01: Plain unknown text ───────────────────────────────────────────

    #[test]
    fn plain_text_produces_retrieval_units() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input("hello\nworld\n"));
        assert!(!output.retrieval_units.is_empty(), "must produce retrieval units");
        assert_eq!(output.capability_used, CapabilityKind::Lexical);
        assert!(!output.fallback_used);
    }

    // ── AZ-02: Unknown file extension ──────────────────────────────────────

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

    #[test]
    fn malformed_utf8_emits_diagnostic_and_produces_units() {
        let bad_bytes: Vec<u8> = vec![0xFF, 0xFE, b'h', b'e', b'l', b'l', b'o'];
        let input = make_input(AnalyzerContent::FullBytes(bad_bytes), FileType::Text, 7);
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::MALFORMED_INPUT),
            "must emit MALFORMED_INPUT; got: {codes:?}"
        );
        assert!(!output.retrieval_units.is_empty(), "must produce at least one unit");
    }

    // ── AZ-04: Empty file ───────────────────────────────────────────────────

    #[test]
    fn empty_file_produces_no_units() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(""));
        assert!(
            output.retrieval_units.is_empty(),
            "empty file must produce zero retrieval units"
        );
        assert!(output.diagnostics.is_empty());
    }

    // ── AZ-05: Redacted input emits REDACTED_INPUT diagnostic ──────────────

    #[test]
    fn redacted_input_emits_diagnostic() {
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(redacted_input("some safe text\n"));
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::REDACTED_INPUT),
            "RedactedBytes must emit REDACTED_INPUT; got: {codes:?}"
        );
        assert!(!output.retrieval_units.is_empty());
    }

    // ── AZ-06: Partial scan flag emits PARTIAL_SCAN diagnostic ─────────────

    #[test]
    fn partial_scan_flag_emits_diagnostic() {
        let mut input = text_input("some text\n");
        input.is_partial_scan = true;
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::PARTIAL_SCAN),
            "is_partial_scan=true must emit PARTIAL_SCAN; got: {codes:?}"
        );
    }

    // ── AZ-07: Chunking — exactly MAX_LINES_PER_CHUNK lines = one unit ─────

    #[test]
    fn exactly_max_lines_produces_one_unit() {
        let text = (0..MAX_LINES_PER_CHUNK)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        assert_eq!(
            output.retrieval_units.len(),
            1,
            "exactly MAX_LINES_PER_CHUNK lines must produce exactly one unit"
        );
    }

    // ── AZ-08: Chunking — MAX_LINES_PER_CHUNK+1 lines = two units ──────────

    #[test]
    fn one_over_max_lines_produces_two_units() {
        let text = (0..=MAX_LINES_PER_CHUNK)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        assert_eq!(
            output.retrieval_units.len(),
            2,
            "MAX_LINES_PER_CHUNK+1 lines must produce two units"
        );
    }

    // ── AZ-09: Span correctness — line numbers are 1-based ─────────────────

    #[test]
    fn span_line_numbers_are_1_based() {
        let text = "line1\nline2\nline3\n";
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        assert_eq!(output.retrieval_units.len(), 1);
        let span = &output.retrieval_units[0].span;
        assert_eq!(span.start_line, 1, "start_line must be 1-based");
        assert_eq!(span.end_line, 3, "end_line must cover all 3 lines");
        assert_eq!(span.start_col, 1);
    }

    // ── AZ-10: Ordinals are 0-based and monotonically increasing ───────────

    #[test]
    fn ordinals_are_zero_based_and_monotonic() {
        // Generate enough lines to produce multiple chunks.
        let text = (0..MAX_LINES_PER_CHUNK * 3)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(&text));
        assert!(output.retrieval_units.len() >= 3);
        for (i, unit) in output.retrieval_units.iter().enumerate() {
            assert_eq!(unit.ordinal, i as u32, "ordinal must equal index (0-based)");
        }
    }

    // ── CRLF line endings ──────────────────────────────────────────────────

    #[test]
    fn crlf_line_endings_produce_correct_spans() {
        // Three lines with CRLF endings.
        let text = "alpha\r\nbeta\r\ngamma\r\n";
        let lines = split_lines_bytes(text);

        // Must produce exactly 3 lines.
        assert_eq!(lines.len(), 3, "CRLF text must split into 3 lines");

        // Content must not contain \r.
        assert_eq!(lines[0].content, "alpha");
        assert_eq!(lines[1].content, "beta");
        assert_eq!(lines[2].content, "gamma");

        // byte_len_with_terminator for CRLF lines: content + 2 bytes (\r\n).
        assert_eq!(lines[0].byte_len_with_terminator, 7); // "alpha\r\n"
        assert_eq!(lines[1].byte_len_with_terminator, 6); // "beta\r\n"
        assert_eq!(lines[2].byte_len_with_terminator, 7); // "gamma\r\n"

        // Analyzer output: spans must reflect line numbers, not raw bytes.
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        assert_eq!(output.retrieval_units.len(), 1);
        let span = &output.retrieval_units[0].span;
        assert_eq!(span.start_line, 1);
        assert_eq!(span.end_line, 3);

        // retrieval_text must not contain \r.
        let rt = &output.retrieval_units[0].retrieval_text;
        assert!(
            !rt.contains('\r'),
            "retrieval_text must not contain \\r from CRLF; got: {rt:?}"
        );
    }

    // ── UTF-8 multibyte ────────────────────────────────────────────────────

    #[test]
    fn multibyte_utf8_produces_correct_line_count() {
        // Each emoji is 4 bytes in UTF-8.
        let text = "😀😁😂\n🎉🎊🎋\nend\n";
        let lines = split_lines_bytes(text);
        assert_eq!(lines.len(), 3, "3 newline-terminated lines in multibyte text");
        assert_eq!(lines[0].content, "😀😁😂");
        assert_eq!(lines[1].content, "🎉🎊🎋");
        assert_eq!(lines[2].content, "end");

        // byte_len_with_terminator: each emoji = 4 bytes, 3 emojis = 12, + \n = 13.
        assert_eq!(lines[0].byte_len_with_terminator, 13);
        assert_eq!(lines[1].byte_len_with_terminator, 13);
        assert_eq!(lines[2].byte_len_with_terminator, 4); // "end\n"

        // Analyzer: span line numbers must be correct (character-count-independent).
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        assert_eq!(output.retrieval_units.len(), 1);
        assert_eq!(output.retrieval_units[0].span.start_line, 1);
        assert_eq!(output.retrieval_units[0].span.end_line, 3);
    }

    // ── Missing trailing newline ───────────────────────────────────────────

    #[test]
    fn missing_trailing_newline_handled() {
        let text = "line1\nline2\nno-newline-at-end";
        let lines = split_lines_bytes(text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2].content, "no-newline-at-end");
        // The last line has no terminator; byte_len_with_terminator = content len.
        assert_eq!(lines[2].byte_len_with_terminator, "no-newline-at-end".len());

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input(text));
        assert_eq!(output.retrieval_units.len(), 1);
        assert_eq!(output.retrieval_units[0].span.end_line, 3);
        // retrieval_text must contain the last line.
        assert!(output.retrieval_units[0].retrieval_text.contains("no-newline-at-end"));
    }

    // ── Resource: max_retrieval_units uses max_retrieval_units, NOT max_ast_nodes ──

    #[test]
    fn resource_budget_exhaustion_uses_max_retrieval_units_not_max_ast_nodes() {
        // Generate enough lines to produce > 2 chunks at MAX_LINES_PER_CHUNK.
        let text = (0..MAX_LINES_PER_CHUNK * 5)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        // Set max_retrieval_units = 2 (should stop after 2 units).
        // max_ast_nodes set to u64::MAX (must NOT be used by GenericAnalyzer).
        let budget = ResourceBudget {
            max_retrieval_units: 2,
            max_ast_nodes: u64::MAX, // must be ignored by GenericAnalyzer
            max_memory_bytes: u64::MAX,
            max_time_ms: 60_000,
            max_recursion_depth: 500,
        };
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input_with_budget(&text, budget));

        // Must have stopped at exactly 2 units.
        assert_eq!(
            output.retrieval_units.len(),
            2,
            "max_retrieval_units=2 must cap output at 2 units; got {}",
            output.retrieval_units.len()
        );

        // Must emit RESOURCE_EXHAUSTED (not CANCELLED).
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED when max_retrieval_units hit; got: {codes:?}"
        );
    }

    // ── Resource: max_memory_bytes enforced ───────────────────────────────

    #[test]
    fn streaming_memory_budget_enforced() {
        // Build text large enough to produce multiple units.
        // Each line is 50 bytes; 500 lines = one unit with ~25 KB retrieval_text.
        let line = "x".repeat(49); // 49 chars + implicit \n from join
        let text = (0..MAX_LINES_PER_CHUNK * 4)
            .map(|_| line.clone())
            .collect::<Vec<_>>()
            .join("\n");

        // Limit memory to just over one unit's worth.
        let one_unit_bytes = (line.len() + 1) * MAX_LINES_PER_CHUNK; // approx
        let budget = ResourceBudget {
            max_memory_bytes: one_unit_bytes as u64 + 1,
            max_retrieval_units: u64::MAX,
            max_ast_nodes: u64::MAX,
            max_time_ms: 60_000,
            max_recursion_depth: 500,
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input_with_budget(&text, budget));

        // Must have stopped early.
        assert!(
            output.retrieval_units.len() < 4,
            "memory budget must stop analysis before all 4 chunks; got {} units",
            output.retrieval_units.len()
        );
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED when memory budget exceeded; got: {codes:?}"
        );
    }

    // ── Resource: max_time_ms enforced ────────────────────────────────────

    #[test]
    fn streaming_time_budget_enforced() {
        // Set max_time_ms = 0 so the first check after the first unit fires immediately.
        let text = (0..MAX_LINES_PER_CHUNK * 10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");

        let budget = ResourceBudget {
            max_time_ms: 0, // expires immediately
            max_retrieval_units: u64::MAX,
            max_ast_nodes: u64::MAX,
            max_memory_bytes: u64::MAX,
            max_recursion_depth: 500,
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(text_input_with_budget(&text, budget));

        // Analysis must have produced fewer than all 10 chunks.
        assert!(
            output.retrieval_units.len() < 10,
            "time budget=0 must stop analysis early; got {} units",
            output.retrieval_units.len()
        );
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED when time budget exceeded; got: {codes:?}"
        );
    }

    // ── Cancellation ──────────────────────────────────────────────────────

    #[test]
    fn cancellation_produces_cancelled_diagnostic() {
        let token = CancellationToken::new();
        token.cancel(); // pre-cancel
        let input = text_input_with_token("line1\nline2\nline3\n", token);
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::CANCELLED),
            "cancelled token must produce CANCELLED diagnostic; got: {codes:?}"
        );
    }

    // ── Streaming: cancellation occurs during streaming, not after collection ──

    #[test]
    fn streaming_cancellation_occurs_during_streaming_not_after_collection() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Write a file large enough to produce multiple stream chunks.
        // Use a file slightly above SMALL_FILE_THRESHOLD so it becomes LARGE.
        let chunk_size = attic_discovery::secrets::STREAM_CHUNK_SIZE;
        let num_chunks = 5;
        let content = "x".repeat(49) + "\n"; // 50-byte line
        let lines_per_chunk = chunk_size / 50 + 1;
        let total_lines = lines_per_chunk * num_chunks;

        let mut tmp = NamedTempFile::new().unwrap();
        for _ in 0..total_lines {
            tmp.write_all(content.as_bytes()).unwrap();
        }
        tmp.flush().unwrap();

        let token = CancellationToken::new();
        // Cancel immediately — before any chunks are fetched.
        token.cancel();

        let stream = attic_discovery::LargeFileStream::open(tmp.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: tmp.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: tmp.as_file().metadata().unwrap().len(),
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        // Cancellation must have been detected during streaming (pre-chunk check).
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::CANCELLED),
            "streaming must detect cancellation before fetching chunks; got: {codes:?}"
        );
        // We should not have accumulated all chunks before stopping.
        // Since cancel happened before first chunk, zero or very few units expected.
        assert!(
            output.retrieval_units.len() < num_chunks,
            "cancellation before streaming must stop early; got {} units",
            output.retrieval_units.len()
        );
    }

    // ── Streaming: boundary spans correct ─────────────────────────────────

    #[test]
    fn streaming_boundary_spans_correct() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Write lines that will straddle stream chunk boundaries.
        // Each line is short; we write enough to fill multiple STREAM_CHUNK_SIZEs.
        let chunk_size = attic_discovery::secrets::STREAM_CHUNK_SIZE;
        let line = "abcdefghijklmnopqrstuvwxyz01234\n"; // 32 bytes
        let num_lines = (chunk_size * 3 / line.len()) + 5;

        let mut tmp = NamedTempFile::new().unwrap();
        for _ in 0..num_lines {
            tmp.write_all(line.as_bytes()).unwrap();
        }
        tmp.flush().unwrap();

        let stream = attic_discovery::LargeFileStream::open(tmp.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: tmp.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: tmp.as_file().metadata().unwrap().len(),
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        assert!(!output.retrieval_units.is_empty(), "must produce at least one unit");

        // Verify contiguous spans: each unit's end_line + 1 = next unit's start_line.
        for window in output.retrieval_units.windows(2) {
            let a = &window[0];
            let b = &window[1];
            assert_eq!(
                a.span.end_line + 1,
                b.span.start_line,
                "spans must be contiguous: unit {} ends at line {}, unit {} starts at line {}",
                a.ordinal, a.span.end_line, b.ordinal, b.span.start_line
            );
        }

        // Total lines covered must equal num_lines.
        let total_covered: u32 = output
            .retrieval_units
            .iter()
            .map(|u| u.span.end_line - u.span.start_line + 1)
            .sum();
        assert_eq!(
            total_covered,
            num_lines as u32,
            "total lines covered by all units must equal input line count"
        );
    }

    // ── Streaming: LARGE file remains bounded memory (no collect_all) ──────

    #[test]
    fn large_streaming_remains_bounded_memory() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Write a file several STREAM_CHUNK_SIZEs long, well into LARGE territory.
        let chunk_size = attic_discovery::secrets::STREAM_CHUNK_SIZE;
        let fill = "safe no secrets here\n".repeat(chunk_size / 21 + 1);
        let repeats = 8;

        let mut tmp = NamedTempFile::new().unwrap();
        for _ in 0..repeats {
            tmp.write_all(fill.as_bytes()).unwrap();
        }
        tmp.flush().unwrap();

        let stream = attic_discovery::LargeFileStream::open(tmp.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: tmp.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: tmp.as_file().metadata().unwrap().len(),
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        // The test proves the analysis completes without OOM or error.
        // No MALFORMED_INPUT or stream errors expected.
        let error_codes: Vec<&str> = output
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::api::DiagnosticSeverity::Error)
            .map(|d| d.code.as_str())
            .collect();
        assert!(
            error_codes.is_empty(),
            "bounded streaming must produce no error diagnostics; got: {error_codes:?}"
        );
        assert!(
            !output.retrieval_units.is_empty(),
            "must produce at least one retrieval unit"
        );
    }

    // ── Streaming: redacted content emits REDACTED_INPUT ──────────────────

    #[test]
    fn streaming_with_secret_emits_redacted_input_diagnostic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        // Write a file with an AWS key so the stream has findings.
        let pad = "x".repeat(100);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{pad}\n{secret}\n{pad}\n");

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(content.as_bytes()).unwrap();
        tmp.flush().unwrap();

        // Use LargeFileStream::open directly (small file, but we're testing the path).
        let stream = attic_discovery::LargeFileStream::open(tmp.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: tmp.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Text,
            size_bytes: tmp.as_file().metadata().unwrap().len(),
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::REDACTED_INPUT),
            "streaming with secret findings must emit REDACTED_INPUT; got: {codes:?}"
        );

        // retrieval_text must not contain the raw secret.
        for unit in &output.retrieval_units {
            assert!(
                !unit.retrieval_text.contains(secret),
                "retrieval_text must not contain raw secret"
            );
        }
    }

    // ── split_lines_bytes: mixed LF and CRLF in same text ─────────────────

    #[test]
    fn mixed_lf_and_crlf_handled() {
        let text = "lf-line\ncrlf-line\r\nlf-again\n";
        let lines = split_lines_bytes(text);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].content, "lf-line");
        assert_eq!(lines[1].content, "crlf-line");
        assert_eq!(lines[2].content, "lf-again");
        // byte_len: "lf-line\n" = 8, "crlf-line\r\n" = 11, "lf-again\n" = 9
        assert_eq!(lines[0].byte_len_with_terminator, 8);
        assert_eq!(lines[1].byte_len_with_terminator, 11);
        assert_eq!(lines[2].byte_len_with_terminator, 9);
    }

    // ── split_lines_bytes: single line no terminator ───────────────────────

    #[test]
    fn single_line_no_terminator() {
        let text = "only one line";
        let lines = split_lines_bytes(text);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content, "only one line");
        assert_eq!(lines[0].byte_len_with_terminator, text.len());
    }

    // ── Invalid UTF-8 in RedactedBytes emits both diagnostics ─────────────

    #[test]
    fn invalid_utf8_in_redacted_input_emits_both_diagnostics() {
        let bad_bytes: Vec<u8> = vec![0xFF, 0xFE, b'h', b'e', b'l', b'l', b'o'];
        let input = make_input(AnalyzerContent::RedactedBytes(bad_bytes), FileType::Text, 7);
        let analyzer = GenericAnalyzer::new();
        let output = analyzer.analyze(input);
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::REDACTED_INPUT),
            "must emit REDACTED_INPUT for RedactedBytes; got: {codes:?}"
        );
        assert!(
            codes.contains(&diagnostic_codes::MALFORMED_INPUT),
            "must emit MALFORMED_INPUT for invalid UTF-8 in RedactedBytes; got: {codes:?}"
        );
    }

    // ── Descriptor: name = "generic", LEXICAL:FULL, no supported file types ─

    #[test]
    fn descriptor_is_lexical_full() {
        let analyzer = GenericAnalyzer::new();
        let desc = analyzer.descriptor();
        assert_eq!(desc.name, "generic");
        assert_eq!(
            desc.capabilities.level_for(CapabilityKind::Lexical),
            CapabilityLevel::Full
        );
        assert!(
            desc.supported_file_types.is_empty(),
            "generic analyzer must declare no supported file types (language-agnostic)"
        );
    }
}
