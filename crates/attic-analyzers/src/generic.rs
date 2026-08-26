//! `GenericAnalyzer` — the mandatory language-agnostic fallback analyzer.
//!
//! Capabilities declared: `LEXICAL:FULL` only.
//!
//! ## Source span semantics — 0-based, exclusive end
//!
//! `SourceSpan` fields follow the canonical `attic-core` definition:
//! - `start_line` / `start_col` — 0-based, inclusive.
//! - `end_line` / `end_col`     — 0-based, **exclusive** (half-open interval).
//!
//! A chunk covering file lines 0, 1, 2 (three lines) has
//! `start_line = 0`, `end_line = 3`, `start_col = 0`,
//! `end_col = <byte length of last line content>`.
//!
//! ## Carry byte limit
//!
//! Lines longer than `MAX_CARRY_BYTES` in the streaming path are split at UTF-8
//! character boundaries. Each fragment is emitted as its own virtual line so
//! that memory consumption is bounded regardless of file content.

use std::time::Instant;

use attic_core::{FileOccurrenceId, SourceSpan};
use attic_discovery::LargeFileStream;
use tracing::debug;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, ResourceBudget,
    RetrievalUnitSpec, diagnostic_codes,
};
use crate::cancellation::CancellationToken;

/// Maximum source lines per retrieval unit.
pub const MAX_LINES_PER_CHUNK: usize = 500;

/// Maximum bytes held in the inter-chunk carry buffer.
pub const MAX_CARRY_BYTES: usize = 65_536; // 64 KiB

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
                supported_file_types: vec![],
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
                0,
                start_time,
                &mut cum,
            );
        }

        AnalyzerContent::RedactedBytes(bytes) => {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::REDACTED_INPUT,
                "Input contained secrets that were redacted by Phase 1B; retrieval units reflect redacted content.",
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
                0,
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
        structurally_complete: false,
        capability_used: CapabilityKind::Lexical,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// UTF-8 decode helper
// ─────────────────────────────────────────────────────────────────────────────

fn decode_bytes_lossy(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Line splitter
// ─────────────────────────────────────────────────────────────────────────────

struct ParsedLine {
    content: String,
    #[allow(dead_code)]
    byte_len_with_terminator: usize,
}

/// Split `text` into lines. Does NOT use `str::lines()` (CRLF-aware).
fn split_lines_bytes(text: &str) -> Vec<ParsedLine> {
    let mut result = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;

    while pos < len {
        match memchr_newline(bytes, pos) {
            Some(nl) => {
                let content_end = if nl > pos && bytes[nl - 1] == b'\r' {
                    nl - 1
                } else {
                    nl
                };
                let content = text[pos..content_end].to_string();
                let byte_len_with_terminator = nl + 1 - pos;
                result.push(ParsedLine {
                    content,
                    byte_len_with_terminator,
                });
                pos = nl + 1;
            }
            None => {
                let content = text[pos..].to_string();
                let byte_len_with_terminator = len - pos;
                result.push(ParsedLine {
                    content,
                    byte_len_with_terminator,
                });
                pos = len;
            }
        }
    }

    result
}

#[inline]
fn memchr_newline(bytes: &[u8], from: usize) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|rel| from + rel)
}

/// Find the largest UTF-8 char boundary <= max_bytes within `s`.
#[inline]
fn floor_char_boundary(s: &str, max_bytes: usize) -> usize {
    let cap = max_bytes.min(s.len());
    (0..=cap)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// Build RetrievalUnitSpec — 0-based exclusive-end spans
// ─────────────────────────────────────────────────────────────────────────────

fn build_retrieval_unit_from_lines(
    ordinal: u32,
    start_line_0: u32,
    lines: &[String],
) -> RetrievalUnitSpec {
    let end_line_0 = start_line_0 + lines.len() as u32; // exclusive
    let end_col_0 = lines.last().map(|l| l.len() as u32).unwrap_or(0); // exclusive
    let retrieval_text = lines.join("\n");
    RetrievalUnitSpec {
        span: SourceSpan::new(start_line_0, 0, end_line_0, end_col_0),
        retrieval_text,
        ordinal,
        structural_node_index: None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Resource budget check — returns true if analysis must stop
// ─────────────────────────────────────────────────────────────────────────────

fn check_and_emit_resource(
    units: &[RetrievalUnitSpec],
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    start_time: Instant,
    cumulative_text_bytes: u64,
) -> bool {
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
    if cumulative_text_bytes >= budget.max_memory_bytes {
        diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::RESOURCE_EXHAUSTED,
            format!(
                "max_memory_bytes ({}) exceeded at {} cumulative bytes; output is partial.",
                budget.max_memory_bytes, cumulative_text_bytes
            ),
        ));
        return true;
    }
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
// Buffered path: FullBytes / RedactedBytes
// ─────────────────────────────────────────────────────────────────────────────

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
    let mut chunk_start = 0usize;

    loop {
        if chunk_start >= lines.len() {
            break;
        }
        // Check before building the next chunk.
        if token.is_cancelled() {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Analysis cancelled; output is partial.",
            ));
            return;
        }
        if start_time.elapsed().as_millis() as u64 >= budget.max_time_ms {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "max_time_ms ({} ms) exceeded after {} units; output is partial.",
                    budget.max_time_ms,
                    units.len()
                ),
            ));
            return;
        }

        let chunk_end = (chunk_start + MAX_LINES_PER_CHUNK).min(lines.len());
        let chunk_lines: Vec<String> = lines[chunk_start..chunk_end]
            .iter()
            .map(|l| l.content.clone())
            .collect();
        let start_line_0 = line_number_base + chunk_start as u32;
        let unit = build_retrieval_unit_from_lines(units.len() as u32, start_line_0, &chunk_lines);
        *cumulative_text_bytes += unit.retrieval_text.len() as u64;
        units.push(unit);
        chunk_start = chunk_end;

        if check_and_emit_resource(
            units,
            diagnostics,
            budget,
            token,
            start_time,
            *cumulative_text_bytes,
        ) {
            return;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Streaming path: StreamingHandle
// ─────────────────────────────────────────────────────────────────────────────

/// Consume a `LargeFileStream` chunk-by-chunk, producing retrieval units.
///
/// ## Carry buffer invariant
///
/// Between stream chunks, a `carry` string holds an incomplete final line from
/// the previous chunk. The carry is bounded to `MAX_CARRY_BYTES` bytes:
/// if appending new data would push it over the limit, the oversized fragment
/// is split at a UTF-8 char boundary and emitted as a virtual line before
/// the remainder is stored as the new carry.
fn stream_into_units(
    stream: &mut LargeFileStream,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    start_time: Instant,
) {
    let mut carry = String::new();
    let mut pending_lines: Vec<String> = Vec::new();
    let mut next_line_0: u32 = 0; // 0-based line counter in the reconstructed file
    let mut cumulative_text_bytes: u64 = 0;

    loop {
        // Check cancellation/time BEFORE fetching the next chunk.
        if token.is_cancelled() {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Analysis cancelled; output is partial.",
            ));
            // Flush whatever we have.
            flush_stream_pending(
                &mut pending_lines,
                units,
                diagnostics,
                budget,
                token,
                start_time,
                &mut cumulative_text_bytes,
                &mut next_line_0,
            );
            return;
        }
        if start_time.elapsed().as_millis() as u64 >= budget.max_time_ms {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "max_time_ms ({} ms) exceeded after {} units; output is partial.",
                    budget.max_time_ms,
                    units.len()
                ),
            ));
            flush_stream_pending(
                &mut pending_lines,
                units,
                diagnostics,
                budget,
                token,
                start_time,
                &mut cumulative_text_bytes,
                &mut next_line_0,
            );
            return;
        }

        // Fetch next chunk from the stream.
        let chunk = match stream.next_chunk() {
            None => break, // EOF
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::MALFORMED_INPUT,
                    format!("Stream read error: {e}; output may be partial."),
                ));
                break;
            }
        };

        // `chunk.redacted` is already secret-free.
        // Prepend carry to the chunk text.
        let chunk_text = if carry.is_empty() {
            chunk.redacted
        } else {
            let mut combined = std::mem::take(&mut carry);
            combined.push_str(&chunk.redacted);
            combined
        };

        // Split into lines. The last "line" may be incomplete (no trailing \n).
        let lines = split_lines_bytes(&chunk_text);
        let n = lines.len();

        if n == 0 {
            // chunk_text was empty after prepending carry
            carry = chunk_text;
            continue;
        }

        // Determine if the chunk text ends with a newline.
        // If it does, all lines are complete; otherwise the last is partial.
        let chunk_bytes = chunk_text.as_bytes();
        let ends_with_newline = chunk_bytes.last().map(|&b| b == b'\n').unwrap_or(false);

        let complete_count = if ends_with_newline {
            n
        } else {
            n.saturating_sub(1)
        };

        // Collect complete lines into pending.
        for line in &lines[..complete_count] {
            let content = line.content.clone();
            // Apply MAX_CARRY_BYTES splitting to any oversized line.
            emit_possibly_oversized_line(content, &mut pending_lines);
        }

        // The last partial line becomes the new carry (if any).
        if !ends_with_newline && n > 0 {
            let partial = lines[n - 1].content.clone();
            // Enforce MAX_CARRY_BYTES on carry growth.
            if partial.len() > MAX_CARRY_BYTES {
                // Split the oversized fragment.
                let mut remaining = partial.as_str();
                while remaining.len() > MAX_CARRY_BYTES {
                    let split_at = floor_char_boundary(remaining, MAX_CARRY_BYTES);
                    let fragment = remaining[..split_at].to_string();
                    remaining = &remaining[split_at..];
                    emit_possibly_oversized_line(fragment, &mut pending_lines);
                }
                carry = remaining.to_string();
            } else {
                carry = partial;
            }
        } else {
            carry.clear();
        }

        // Flush complete chunks from pending_lines.
        while pending_lines.len() >= MAX_LINES_PER_CHUNK {
            let chunk_lines: Vec<String> = pending_lines.drain(..MAX_LINES_PER_CHUNK).collect();
            let unit =
                build_retrieval_unit_from_lines(units.len() as u32, next_line_0, &chunk_lines);
            next_line_0 += chunk_lines.len() as u32;
            cumulative_text_bytes += unit.retrieval_text.len() as u64;
            units.push(unit);
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

    // EOF: flush carry into pending_lines.
    if !carry.is_empty() {
        let leftover = std::mem::take(&mut carry);
        emit_possibly_oversized_line(leftover, &mut pending_lines);
    }

    // Flush all remaining pending_lines.
    flush_stream_pending(
        &mut pending_lines,
        units,
        diagnostics,
        budget,
        token,
        start_time,
        &mut cumulative_text_bytes,
        &mut next_line_0,
    );
}

/// Push a line into `pending_lines`, splitting it at `MAX_CARRY_BYTES`
/// boundaries if it is oversized. This enforces bounded memory for files
/// that contain no newlines or have arbitrarily long lines.
fn emit_possibly_oversized_line(line: String, pending: &mut Vec<String>) {
    if line.len() <= MAX_CARRY_BYTES {
        pending.push(line);
        return;
    }
    // Split into MAX_CARRY_BYTES-sized fragments.
    let mut remaining = line.as_str();
    while remaining.len() > MAX_CARRY_BYTES {
        let split_at = floor_char_boundary(remaining, MAX_CARRY_BYTES);
        let fragment = remaining[..split_at].to_string();
        remaining = &remaining[split_at..];
        pending.push(fragment);
    }
    if !remaining.is_empty() {
        pending.push(remaining.to_string());
    }
}

/// Flush all lines in `pending_lines` into retrieval units, respecting limits.
#[allow(clippy::too_many_arguments)]
fn flush_stream_pending(
    pending_lines: &mut Vec<String>,
    units: &mut Vec<RetrievalUnitSpec>,
    diagnostics: &mut Vec<AnalyzerDiagnostic>,
    budget: &ResourceBudget,
    token: &CancellationToken,
    start_time: Instant,
    cumulative_text_bytes: &mut u64,
    next_line_0: &mut u32,
) {
    while !pending_lines.is_empty() {
        if token.is_cancelled() {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "Analysis cancelled; output is partial.",
            ));
            pending_lines.clear();
            return;
        }
        if start_time.elapsed().as_millis() as u64 >= budget.max_time_ms {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "max_time_ms ({} ms) exceeded; output is partial.",
                    budget.max_time_ms
                ),
            ));
            pending_lines.clear();
            return;
        }
        let take = MAX_LINES_PER_CHUNK.min(pending_lines.len());
        let chunk_lines: Vec<String> = pending_lines.drain(..take).collect();
        let unit = build_retrieval_unit_from_lines(units.len() as u32, *next_line_0, &chunk_lines);
        *next_line_0 += chunk_lines.len() as u32;
        *cumulative_text_bytes += unit.retrieval_text.len() as u64;
        units.push(unit);
        if check_and_emit_resource(
            units,
            diagnostics,
            budget,
            token,
            start_time,
            *cumulative_text_bytes,
        ) {
            pending_lines.clear();
            return;
        }
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

    use crate::api::{AnalyzerContent, AnalyzerInput, ResourceBudget};
    use crate::cancellation::CancellationToken;

    // ── Helpers ──────────────────────────────────────────────────────────────

    fn make_input(content: AnalyzerContent, size_bytes: u64) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("test.txt"),
            content,
            language_hint: None,
            file_type: FileType::Text,
            size_bytes,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        }
    }

    fn text_input(text: &str) -> AnalyzerInput {
        make_input(
            AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            text.len() as u64,
        )
    }

    fn redacted_input(text: &str) -> AnalyzerInput {
        make_input(
            AnalyzerContent::RedactedBytes(text.as_bytes().to_vec()),
            text.len() as u64,
        )
    }

    fn analyzer() -> GenericAnalyzer {
        GenericAnalyzer::new()
    }

    // ── Empty file ────────────────────────────────────────────────────────────

    #[test]
    fn empty_file_produces_no_units() {
        let out = analyzer().analyze(text_input(""));
        assert!(
            out.retrieval_units.is_empty(),
            "empty file must produce zero units"
        );
        assert!(out.diagnostics.is_empty());
    }

    // ── Single-chunk: LF ──────────────────────────────────────────────────────

    #[test]
    fn three_lines_lf_span_0_based_exclusive() {
        let text = "line1\nline2\nline3\n";
        let out = analyzer().analyze(text_input(text));
        assert_eq!(out.retrieval_units.len(), 1);
        let u = &out.retrieval_units[0];
        // 0-based: lines 0, 1, 2 → start=0 (inclusive), end=3 (exclusive)
        assert_eq!(u.span.start_line, 0, "start_line must be 0");
        assert_eq!(u.span.start_col, 0, "start_col must be 0");
        assert_eq!(u.span.end_line, 3, "end_line must be 3 (exclusive)");
        assert_eq!(u.span.end_col, 5, "end_col must be len('line3')=5");
        assert_eq!(u.ordinal, 0);
    }

    // ── Single-chunk: CRLF ────────────────────────────────────────────────────

    #[test]
    fn three_lines_crlf_span_0_based_exclusive() {
        let text = "line1\r\nline2\r\nline3\r\n";
        let out = analyzer().analyze(text_input(text));
        assert_eq!(out.retrieval_units.len(), 1);
        let u = &out.retrieval_units[0];
        assert_eq!(u.span.start_line, 0);
        assert_eq!(u.span.start_col, 0);
        assert_eq!(u.span.end_line, 3, "CRLF: end_line must be 3 (exclusive)");
        assert_eq!(u.span.end_col, 5, "CRLF: end_col must be len('line3')=5");
    }

    // ── Unterminated final line ───────────────────────────────────────────────

    #[test]
    fn unterminated_line_produces_one_unit() {
        let text = "only line no newline";
        let out = analyzer().analyze(text_input(text));
        assert_eq!(out.retrieval_units.len(), 1);
        let u = &out.retrieval_units[0];
        assert_eq!(u.span.start_line, 0);
        assert_eq!(u.span.end_line, 1, "one line → end_line=1 (exclusive)");
        assert_eq!(u.span.end_col, text.len() as u32);
    }

    // ── Multi-chunk splitting ─────────────────────────────────────────────────

    #[test]
    fn more_than_max_lines_produces_multiple_chunks() {
        let line = "x\n";
        let n = MAX_LINES_PER_CHUNK + 3;
        let text: String = line.repeat(n);
        let out = analyzer().analyze(text_input(&text));
        assert_eq!(out.retrieval_units.len(), 2, "must split into 2 chunks");

        let u0 = &out.retrieval_units[0];
        assert_eq!(u0.span.start_line, 0);
        assert_eq!(u0.span.end_line, MAX_LINES_PER_CHUNK as u32);
        assert_eq!(u0.ordinal, 0);

        let u1 = &out.retrieval_units[1];
        assert_eq!(u1.span.start_line, MAX_LINES_PER_CHUNK as u32);
        assert_eq!(u1.span.end_line, n as u32);
        assert_eq!(u1.ordinal, 1);
    }

    // ── RedactedBytes ─────────────────────────────────────────────────────────

    #[test]
    fn redacted_bytes_emits_redacted_input_diagnostic() {
        let out = analyzer().analyze(redacted_input("hello\nworld\n"));
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::REDACTED_INPUT),
            "must emit REDACTED_INPUT; got {codes:?}"
        );
        assert!(
            !out.retrieval_units.is_empty(),
            "redacted input must still produce units"
        );
    }

    // ── Invalid UTF-8 ─────────────────────────────────────────────────────────

    #[test]
    fn invalid_utf8_emits_malformed_input_diagnostic() {
        let bytes = vec![0xFF, 0xFE, b'h', b'i', b'\n'];
        let out = analyzer().analyze(make_input(AnalyzerContent::FullBytes(bytes), 5));
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::MALFORMED_INPUT),
            "must emit MALFORMED_INPUT; got {codes:?}"
        );
        assert!(
            !out.retrieval_units.is_empty(),
            "invalid UTF-8 must still produce units"
        );
    }

    // ── Cancellation ──────────────────────────────────────────────────────────

    #[test]
    fn cancellation_stops_analysis() {
        // Pre-cancel the token before calling analyze.
        let token = CancellationToken::new();
        token.cancel();

        let n = MAX_LINES_PER_CHUNK * 4;
        let text: String = "line\n".repeat(n);
        let mut input = text_input(&text);
        input.cancellation_token = token;

        let out = GenericAnalyzer::new().analyze(input);

        // Must have emitted CANCELLED diagnostic.
        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::CANCELLED),
            "pre-cancelled input must emit CANCELLED; got: {codes:?}"
        );
        // May produce zero or partial units — either is acceptable, but must
        // not have processed all 4 × MAX_LINES_PER_CHUNK lines.
        assert!(
            out.retrieval_units.len() < 4,
            "cancelled analysis should not produce all 4 chunks; got {}",
            out.retrieval_units.len()
        );
    }

    // ── Resource limits ───────────────────────────────────────────────────────

    #[test]
    fn resource_limit_retrieval_units() {
        // Set max_retrieval_units = 1 with content that would otherwise produce 3+.
        let n = MAX_LINES_PER_CHUNK * 3 + 1;
        let text: String = "x\n".repeat(n);

        let mut input = text_input(&text);
        input.resource_budget.max_retrieval_units = 1;

        let out = GenericAnalyzer::new().analyze(input);

        // Must have stopped at 1 unit.
        assert_eq!(
            out.retrieval_units.len(),
            1,
            "must stop at max_retrieval_units=1"
        );

        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED when unit limit hit; got: {codes:?}"
        );
    }

    #[test]
    fn resource_limit_memory_bytes() {
        // Set max_memory_bytes very low (1 byte) so the first unit exceeds it.
        let text = "hello\nworld\n";
        let mut input = text_input(text);
        input.resource_budget.max_memory_bytes = 1;

        let out = GenericAnalyzer::new().analyze(input);

        let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::RESOURCE_EXHAUSTED),
            "must emit RESOURCE_EXHAUSTED when memory limit hit; got: {codes:?}"
        );
    }

    // ── Streaming — basic units ───────────────────────────────────────────────

    #[test]
    fn streaming_handle_produces_units() {
        use attic_discovery::LargeFileStream;
        use std::io::Write as IoWrite;
        use tempfile::NamedTempFile;

        let mut f = NamedTempFile::new().unwrap();
        let content = "line one\nline two\nline three\n";
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();

        let stream = LargeFileStream::open(f.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: attic_core::FileOccurrenceId::new_v4(),
            path: f.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: attic_core::FileType::Text,
            size_bytes: content.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: crate::api::ResourceBudget::default(),
        };

        let out = GenericAnalyzer::new().analyze(input);
        assert!(
            !out.retrieval_units.is_empty(),
            "streaming input must produce retrieval units"
        );
    }

    // ── Streaming — bounded carry (no OOM on huge no-newline line) ─────────────

    #[test]
    fn streaming_bounded_carry_no_oom() {
        use attic_discovery::LargeFileStream;
        use std::io::Write as IoWrite;
        use tempfile::NamedTempFile;

        // Write a file with no newlines, larger than MAX_CARRY_BYTES.
        let size = MAX_CARRY_BYTES * 2 + 1;
        let mut f = NamedTempFile::new().unwrap();
        let chunk = b"x".repeat(4096);
        let mut written = 0usize;
        while written < size {
            let to_write = chunk.len().min(size - written);
            f.write_all(&chunk[..to_write]).unwrap();
            written += to_write;
        }
        f.flush().unwrap();

        let stream = LargeFileStream::open(f.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: attic_core::FileOccurrenceId::new_v4(),
            path: f.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: attic_core::FileType::Text,
            size_bytes: size as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: crate::api::ResourceBudget::default(),
        };

        // Must not panic or hang; must produce at least one retrieval unit.
        let out = GenericAnalyzer::new().analyze(input);
        assert!(
            !out.retrieval_units.is_empty(),
            "no-newline LARGE input must still produce at least one unit"
        );
        // Verify units are non-empty slices of the content.
        for u in &out.retrieval_units {
            assert!(
                !u.retrieval_text.is_empty(),
                "each retrieval unit must have non-empty text"
            );
        }
    }

    // ── Streaming — 0-based exclusive-end spans ───────────────────────────────

    #[test]
    fn streaming_span_0_based_exclusive() {
        use attic_discovery::LargeFileStream;
        use std::io::Write as IoWrite;
        use tempfile::NamedTempFile;

        // Three lines; all within one chunk so span covers lines 0..3.
        let content = "alpha\nbeta\ngamma\n";
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();

        let stream = LargeFileStream::open(f.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: attic_core::FileOccurrenceId::new_v4(),
            path: f.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: attic_core::FileType::Text,
            size_bytes: content.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: crate::api::ResourceBudget::default(),
        };

        let out = GenericAnalyzer::new().analyze(input);
        assert_eq!(
            out.retrieval_units.len(),
            1,
            "3 lines must produce exactly 1 unit"
        );

        let u = &out.retrieval_units[0];
        assert_eq!(
            u.span.start_line, 0,
            "streaming: start_line must be 0-based"
        );
        assert_eq!(u.span.start_col, 0, "streaming: start_col must be 0");
        assert_eq!(
            u.span.end_line, 3,
            "streaming: end_line must be exclusive (3 for 3 lines)"
        );
        // end_col = len("gamma") = 5
        assert_eq!(
            u.span.end_col, 5,
            "streaming: end_col must be len of last line"
        );
    }

    // ── floor_char_boundary ───────────────────────────────────────────────────

    #[test]
    fn floor_char_boundary_splits_at_utf8_boundary() {
        // "é" is 2 bytes (0xC3 0xA9).  A string "aéb" is 4 bytes.
        // floor_char_boundary("aéb", 2) must return 1 (before "é"), not 2
        // (which would split the multibyte sequence).
        let s = "aéb"; // bytes: [97, 195, 169, 98]
        assert_eq!(s.len(), 4);

        // At max_bytes=1: only "a" (1 byte) fits → boundary = 1.
        assert_eq!(floor_char_boundary(s, 1), 1);

        // At max_bytes=2: "é" starts at byte 1 and is 2 bytes (ends at 3).
        // Splitting at 2 would cut inside "é", so must return 1.
        assert_eq!(floor_char_boundary(s, 2), 1);

        // At max_bytes=3: "é" ends at byte 3 → full char fits → boundary = 3.
        assert_eq!(floor_char_boundary(s, 3), 3);

        // At max_bytes >= len: full string → boundary = len.
        assert_eq!(floor_char_boundary(s, 10), s.len());

        // Pure ASCII: every byte offset is a valid char boundary.
        let ascii = "hello";
        assert_eq!(floor_char_boundary(ascii, 3), 3);
        assert_eq!(floor_char_boundary(ascii, 0), 0);

        // 3-byte UTF-8: "€" = [0xE2, 0x82, 0xAC] (3 bytes).
        let euro = "€x"; // bytes: [226, 130, 172, 120]
        assert_eq!(floor_char_boundary(euro, 1), 0);
        assert_eq!(floor_char_boundary(euro, 2), 0);
        assert_eq!(floor_char_boundary(euro, 3), 3);
        assert_eq!(floor_char_boundary(euro, 4), 4);
    }
}
