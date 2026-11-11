//! JSON content-class analyzer (Phase 3 — corpus-aware routing).
//!
//! The GenericAnalyzer's 2,000-char line chunks are wrong for large JSON
//! documents: chunk boundaries depend on incidental whitespace/indentation,
//! so identical subtrees in DEV/QA/STAGE/UAT/PROD exports produce *different*
//! chunks and defeat content-hash dedup (measured: 87.6% of lines duplicate
//! across those five files).
//!
//! This analyzer instead:
//! - parses the document,
//! - decomposes it into top-level subtrees (object members / array elements),
//! - serializes each subtree CANONICALLY (compact, key-sorted) — identical
//!   logical content yields byte-identical text regardless of source
//!   formatting, so `selection`'s content-hash dedup (EX_DUPLICATE) collapses
//!   shared subtrees across environment files,
//! - prefixes each unit with its JSON pointer for exact addressing,
//! - splits oversized subtrees recursively (never emitting a unit beyond the
//!   resource budget), keeping every byte represented,
//! - emits MALFORMED_INPUT and yields nothing on parse failure — `dispatch`
//!   then falls back to GenericAnalyzer, so malformed JSON is still fully
//!   indexed as plain text.

use attic_core::{FileType, SourceSpan};
use std::collections::HashSet;
use tracing::debug;

#[path = "json_spans.rs"]
pub mod json_spans;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, RetrievalUnitSpec,
    diagnostic_codes,
};
use crate::generic::TARGET_CHUNK_CHARS;

/// JSON analyzer: canonical subtree chunking with JSON-pointer addressing.
pub struct JsonAnalyzer {
    desc: AnalyzerDescriptor,
}

impl JsonAnalyzer {
    pub fn new() -> Self {
        Self {
            desc: AnalyzerDescriptor {
                name: "json".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description:
                    "JSON-aware analyzer: canonical subtree chunks with JSON-pointer addressing."
                        .to_string(),
                supported_file_types: vec![FileType::Json],
                capabilities: AnalyzerCapabilities::single(
                    CapabilityKind::Lexical,
                    CapabilityLevel::Full,
                ),
            },
        }
    }
}

impl Default for JsonAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer for JsonAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.desc
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        let start = std::time::Instant::now();
        let mut diagnostics: Vec<AnalyzerDiagnostic> = Vec::new();
        let mut units: Vec<RetrievalUnitSpec> = Vec::new();
        let mut pointers = Vec::new();
        let position_basis = match &input.content {
            AnalyzerContent::FullBytes(_) => "source",
            AnalyzerContent::RedactedBytes(_) | AnalyzerContent::StreamingHandle(_) => "processed",
        };

        // This analyzer requires whole-content access (JSON parsing is not
        // line-streamable without a much larger streaming-JSON investment).
        // LARGE/StreamingHandle inputs are drained into memory — the largest
        // corpus JSON seen is ~5 MB, well within budget.
        let file_occurrence_id = input.file_occurrence_id;
        let bytes: Vec<u8> = match input.content {
            AnalyzerContent::FullBytes(b) | AnalyzerContent::RedactedBytes(b) => b,
            AnalyzerContent::StreamingHandle(mut stream) => {
                // LargeFileStream is chunked, not std::io::Read — drain it,
                // but never past the memory budget or time budget, and
                // honour cancellation between chunks. Exceeding a limit is an
                // ERROR so dispatch falls back to GenericAnalyzer, which
                // streams the file and still indexes every byte.
                // A parsed serde_json::Value costs several times its source
                // size, so the raw document may use a quarter of the budget.
                let max_bytes = usize::try_from(input.resource_budget.max_memory_bytes / 4)
                    .unwrap_or(usize::MAX)
                    .max(1);
                let max_ms = input.resource_budget.max_time_ms;
                let mut buf = Vec::new();
                let mut abort: Option<AnalyzerDiagnostic> = None;
                loop {
                    if input.cancellation_token.is_cancelled() {
                        abort = Some(AnalyzerDiagnostic::warning(
                            diagnostic_codes::CANCELLED,
                            "JSON analysis cancelled while reading the stream",
                        ));
                        break;
                    }
                    if max_ms > 0 && start.elapsed().as_millis() as u64 > max_ms {
                        abort = Some(AnalyzerDiagnostic::error(
                            diagnostic_codes::RESOURCE_EXHAUSTED,
                            format!(
                                "JSON stream read exceeded the {max_ms} ms time budget; falling back to plain-text chunking"
                            ),
                        ));
                        break;
                    }
                    match stream.next_chunk() {
                        Some(Ok(chunk)) => {
                            let bytes = chunk.redacted.as_bytes();
                            if buf.len().saturating_add(bytes.len()) > max_bytes {
                                abort = Some(AnalyzerDiagnostic::error(
                                    diagnostic_codes::RESOURCE_EXHAUSTED,
                                    format!(
                                        "JSON document exceeds the {max_bytes}-byte memory budget; falling back to plain-text chunking"
                                    ),
                                ));
                                break;
                            }
                            buf.extend_from_slice(bytes);
                        }
                        Some(Err(e)) => {
                            abort = Some(AnalyzerDiagnostic::error(
                                diagnostic_codes::MALFORMED_INPUT,
                                format!("failed to read JSON stream: {e}"),
                            ));
                            break;
                        }
                        None => break,
                    }
                }
                if let Some(d) = abort {
                    diagnostics.push(d);
                    return self.empty_output(file_occurrence_id, diagnostics);
                }
                buf
            }
        };

        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                // Error severity: dispatch must fall back to GenericAnalyzer
                // so the bytes are still fully indexed as plain text.
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::MALFORMED_INPUT,
                    "JSON input is not valid UTF-8; falling back to generic chunking.",
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            }
        };

        let value: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                // Malformed JSON: emit an ERROR diagnostic and produce NO
                // units — dispatch falls back to GenericAnalyzer (fallback is
                // keyed on error severity) so every byte is still indexed as
                // plain text, and the file is reported.
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::MALFORMED_INPUT,
                    format!("JSON parse failed ({e}); falling back to plain-text chunking"),
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            }
        };

        // Environment label from the filename (DEV/QA/STAGE/UAT/PROD) — lets
        // a unit carry which environment export it came from.
        let env_label = input
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(detect_env_label);

        let max_units = input.resource_budget.max_retrieval_units as usize;
        let mut state = ChunkState {
            units: &mut units,
            pointers: &mut pointers,
            diagnostics: &mut diagnostics,
            max_units,
            env: env_label.as_deref(),
            cancelled: false,
            budget_reported: false,
            depth: 0,
            max_depth: input.resource_budget.max_recursion_depth.max(1),
            depth_reported: false,
        };
        chunk_value(&value, "", &mut state, 0, &input.cancellation_token);
        if !state.cancelled && !state.budget_reported {
            let mut group_budget = GroupBudget {
                remaining_units: state.max_units.saturating_sub(state.units.len()).min(512),
                remaining_bytes: usize::try_from(input.resource_budget.max_memory_bytes / 8)
                    .unwrap_or(usize::MAX),
                skipped: false,
            };
            add_array_groups(
                &value,
                "",
                &mut state,
                &mut group_budget,
                0,
                start,
                input.resource_budget.max_time_ms,
                &input.cancellation_token,
            );
            if group_budget.skipped {
                state.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::UNIT_TRUNCATED,
                    "Optional JSON array groups exceeded their budget; complete leaf units remain indexed",
                ));
            }
        }
        let cancelled = state.cancelled;
        if cancelled {
            diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::CANCELLED,
                "JSON analysis cancelled; output is partial",
            ));
            return self.empty_output(file_occurrence_id, diagnostics);
        }

        let wanted: HashSet<String> = pointers.iter().cloned().collect();
        let locations = match json_spans::locate(
            &text,
            &wanted,
            start,
            input.resource_budget.max_time_ms,
            &input.cancellation_token,
        ) {
            Ok(locations) => locations,
            Err(error) => {
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    format!("{error}; falling back to plain-text chunking"),
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            }
        };
        let mut line_starts = vec![0usize];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset + 1)),
        );
        for (unit, pointer) in units.iter_mut().zip(&pointers) {
            let Some(location) = locations.get(pointer) else {
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::MALFORMED_INPUT,
                    format!("JSON pointer {pointer:?} has no source location; falling back to plain-text chunking"),
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            };
            let start_line = line_starts.partition_point(|offset| *offset <= location.start) - 1;
            let last_byte = location.end.saturating_sub(1).max(location.start);
            let last_line = line_starts.partition_point(|offset| *offset <= last_byte) - 1;
            let Some((start_line_u32, start_col, end_line, end_col)) = u32::try_from(start_line)
                .ok()
                .zip(u32::try_from(location.start - line_starts[start_line]).ok())
                .zip(u32::try_from(last_line + 1).ok())
                .zip(u32::try_from(location.end - line_starts[last_line]).ok())
                .map(|(((start_line, start_col), end_line), end_col)| {
                    (start_line, start_col, end_line, end_col)
                })
            else {
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    "JSON location exceeds supported coordinate range; falling back to plain-text chunking",
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            };
            unit.span = SourceSpan {
                start_line: start_line_u32,
                start_col,
                end_line,
                end_col,
            };
            let Some(metadata) = unit
                .occurrence_metadata
                .as_deref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .and_then(|value| value.as_object().cloned())
            else {
                diagnostics.push(AnalyzerDiagnostic::error(
                    diagnostic_codes::MALFORMED_INPUT,
                    "JSON unit metadata is invalid; falling back to plain-text chunking",
                ));
                return self.empty_output(file_occurrence_id, diagnostics);
            };
            let mut metadata = metadata;
            metadata.insert("source_start_byte".into(), location.start.into());
            metadata.insert("source_end_byte".into(), location.end.into());
            metadata.insert("position_basis".into(), position_basis.into());
            metadata.insert("position_kind".into(), "enclosing_value".into());
            unit.occurrence_metadata = Some(serde_json::Value::Object(metadata).to_string());
        }

        debug!(
            path = %input.path.display(),
            units = units.len(),
            elapsed_ms = start.elapsed().as_millis(),
            "JsonAnalyzer: analysis complete"
        );

        AnalyzerOutput {
            analyzer_id: "json".to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![],
            imports: vec![],
            relationships: vec![],
            retrieval_units: units,
            diagnostics,
            fallback_used: false,
            structurally_complete: true,
            capability_used: CapabilityKind::Lexical,
        }
    }
}

impl JsonAnalyzer {
    fn empty_output(
        &self,
        file_occurrence_id: attic_core::FileOccurrenceId,
        diagnostics: Vec<AnalyzerDiagnostic>,
    ) -> AnalyzerOutput {
        AnalyzerOutput {
            analyzer_id: "json".to_string(),
            analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
            file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![],
            imports: vec![],
            relationships: vec![],
            retrieval_units: vec![],
            diagnostics,
            fallback_used: false,
            structurally_complete: false,
            capability_used: CapabilityKind::Lexical,
        }
    }
}

/// Detect an environment label from a filename stem (DEV/QA/STAGE/UAT/PROD).
fn detect_env_label(filename: &str) -> Option<String> {
    let upper = filename.to_ascii_uppercase();
    for env in ["PROD", "STAGE", "UAT", "QA", "DEV"] {
        // Match as a filename component, not a substring (e.g. "DEVELOPMENT"
        // must not match DEV).
        if upper.split(['-', '_', '.']).any(|part| part == env) {
            return Some(env.to_string());
        }
    }
    None
}

struct ChunkState<'a> {
    units: &'a mut Vec<RetrievalUnitSpec>,
    pointers: &'a mut Vec<String>,
    diagnostics: &'a mut Vec<AnalyzerDiagnostic>,
    max_units: usize,
    env: Option<&'a str>,
    cancelled: bool,
    /// True once a RESOURCE_EXHAUSTED diagnostic has been emitted for the
    /// unit budget — the budget can be hit at many recursion levels; the
    /// diagnostic must appear exactly once, not once per aborted node.
    budget_reported: bool,
    /// Current / maximum subtree recursion depth (`max_recursion_depth`).
    depth: u32,
    max_depth: u32,
    depth_reported: bool,
}

impl ChunkState<'_> {
    /// Record that the retrieval-unit budget stopped analysis before every
    /// subtree was represented. The output is INCOMPLETE by definition; the
    /// indexing layer treats this code as a hard completeness failure that
    /// must block generation publication (fail-closed), never as a silent
    /// truncation.
    fn report_budget_exhausted(&mut self) {
        if self.budget_reported {
            return;
        }
        self.budget_reported = true;
        self.diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::RESOURCE_EXHAUSTED,
            format!(
                "retrieval-unit budget ({} units) exhausted before the whole JSON document was chunked; output is incomplete",
                self.max_units
            ),
        ));
    }
}

/// Serialize a JSON value canonically: compact, object keys sorted. Identical
/// logical content then yields byte-identical text across source files whose
/// formatting differs (the whole point — content-hash dedup downstream).
fn canonical(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    // Serializing a `String` / `Value` to JSON text cannot fail.
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        canonical(&map[k])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(arr) => {
            let inner: Vec<String> = arr.iter().map(canonical).collect();
            format!("[{}]", inner.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Escape a path segment per RFC 6901 JSON Pointer.
fn escape_pointer(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

fn chunk_value(
    value: &serde_json::Value,
    pointer: &str,
    state: &mut ChunkState,
    ordinal_start: u32,
    cancel: &crate::cancellation::CancellationToken,
) {
    if cancel.is_cancelled() {
        state.cancelled = true;
        return;
    }
    if state.units.len() >= state.max_units {
        state.report_budget_exhausted();
        return;
    }

    let body = canonical(value);
    if state.depth >= state.max_depth {
        // Too deep to keep decomposing: emit this subtree as text pieces
        // (every byte still represented) instead of recursing further.
        if !state.depth_reported {
            state.depth_reported = true;
            // Every byte is still indexed, so this is UNIT_TRUNCATED (unit
            // boundaries are synthetic), not RESOURCE_EXHAUSTED, which would
            // mark the output incomplete and block publication.
            state.diagnostics.push(AnalyzerDiagnostic::warning(
                diagnostic_codes::UNIT_TRUNCATED,
                format!(
                    "JSON nesting exceeds max_recursion_depth ({}); deeper subtrees are chunked as text",
                    state.max_depth
                ),
            ));
        }
        let header = format!("// json-pointer: {}\n", pointer);
        push_text_pieces(state, &header, &body, ordinal_start, pointer);
        return;
    }
    let header = match state.env {
        Some(env) => format!("// json-pointer: {} (env: {})\n", pointer, env),
        None => format!("// json-pointer: {}\n", pointer),
    };

    if header.len() + body.len() <= TARGET_CHUNK_CHARS {
        push_unit(state, header, &body, ordinal_start, pointer, None);
        return;
    }

    // Oversized subtree: decompose into children so units stay within the
    // character target and dedup works at finer granularity.
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                let child_ptr = format!("{}/{}", pointer, escape_pointer(k));
                state.depth += 1;
                chunk_value(&map[k], &child_ptr, state, ordinal_start, cancel);
                state.depth -= 1;
                if state.cancelled {
                    return;
                }
                if state.units.len() >= state.max_units {
                    state.report_budget_exhausted();
                    return;
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let child_ptr = format!("{}/{}", pointer, i);
                state.depth += 1;
                chunk_value(item, &child_ptr, state, ordinal_start, cancel);
                state.depth -= 1;
                if state.cancelled {
                    return;
                }
                if state.units.len() >= state.max_units {
                    state.report_budget_exhausted();
                    return;
                }
            }
        }
        // A scalar/primitive that alone exceeds the target (e.g. a huge
        // base64 blob): split the canonical BODY at char boundaries rather
        // than drop it — every byte stays represented, and the header only
        // decorates the first piece's retrieval_text.
        _ => push_text_pieces(state, &header, &body, ordinal_start, pointer),
    }
}

/// Emit `body` as consecutive char-boundary-safe pieces of at most
/// `TARGET_CHUNK_CHARS` (header on the first piece only), stopping at the
/// unit budget.
fn push_text_pieces(
    state: &mut ChunkState,
    header: &str,
    body: &str,
    ordinal_start: u32,
    pointer: &str,
) {
    let mut offset = 0usize;
    let mut first = true;
    while offset < body.len() && state.units.len() < state.max_units {
        let piece_header = if first && header.len() <= TARGET_CHUNK_CHARS - 4 {
            header
        } else {
            ""
        };
        let capacity = TARGET_CHUNK_CHARS - piece_header.len();
        let mut end = (offset + capacity).min(body.len());
        // MSRV 1.89 has no str::floor_char_boundary — walk back to
        // the nearest UTF-8 char boundary manually.
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        push_unit(
            state,
            piece_header.to_string(),
            &body[offset..end],
            ordinal_start,
            pointer,
            None,
        );
        first = false;
        offset = end;
    }
    if offset < body.len() {
        state.report_budget_exhausted();
    }
}

fn push_unit(
    state: &mut ChunkState,
    header: String,
    body: &str,
    ordinal_start: u32,
    pointer: &str,
    group_range: Option<(usize, usize)>,
) {
    let ordinal = ordinal_start + state.units.len() as u32;
    let text = format!("{header}{body}");
    let end_line = body.matches('\n').count() as u32;
    // Canonical body excludes the pointer/env header so identical logical
    // content hashes identically across files and environments (r03). The
    // header lives only in retrieval_text (lexical display) and the
    // occurrence metadata JSON (provenance for filtering/display).
    let mut occurrence_metadata = match state.env {
        Some(env) => serde_json::json!({"json_pointer": pointer, "environment": env}),
        None => serde_json::json!({"json_pointer": pointer}),
    };
    if let Some((start, end)) = group_range {
        occurrence_metadata["array_start"] = start.into();
        occurrence_metadata["array_end"] = end.into();
    }
    state.units.push(RetrievalUnitSpec {
        span: SourceSpan {
            start_line: 0,
            start_col: 0,
            end_line: end_line.max(1),
            end_col: 0,
        },
        retrieval_text: text,
        canonical_text: Some(body.to_string()),
        occurrence_metadata: Some(occurrence_metadata.to_string()),
        ordinal,
        structural_node_index: None,
    });
    state.pointers.push(pointer.to_string());
}

struct GroupBudget {
    remaining_units: usize,
    remaining_bytes: usize,
    skipped: bool,
}

fn emit_array_group(
    state: &mut ChunkState,
    budget: &mut GroupBudget,
    pointer: &str,
    header: &str,
    elements: &[String],
    first: usize,
) {
    if elements.len() < 2 {
        return;
    }
    let body = format!("[{}]", elements.join(","));
    let size = header.len() + body.len();
    if budget.remaining_units == 0 || size > budget.remaining_bytes {
        budget.skipped = true;
        return;
    }
    push_unit(
        state,
        header.to_string(),
        &body,
        0,
        pointer,
        Some((first, first + elements.len())),
    );
    budget.remaining_units -= 1;
    budget.remaining_bytes -= size;
}

#[allow(clippy::too_many_arguments)]
fn add_array_groups(
    value: &serde_json::Value,
    pointer: &str,
    state: &mut ChunkState,
    budget: &mut GroupBudget,
    depth: u32,
    started: std::time::Instant,
    max_time_ms: u64,
    cancel: &crate::cancellation::CancellationToken,
) {
    if cancel.is_cancelled() {
        state.cancelled = true;
        return;
    }
    if !value.is_array() && !value.is_object() {
        return;
    }
    if depth >= state.max_depth
        || (max_time_ms > 0 && started.elapsed().as_millis() > max_time_ms as u128)
    {
        budget.skipped = true;
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                let child_pointer = format!("{pointer}/{}", escape_pointer(key));
                add_array_groups(
                    child,
                    &child_pointer,
                    state,
                    budget,
                    depth + 1,
                    started,
                    max_time_ms,
                    cancel,
                );
                if state.cancelled {
                    return;
                }
            }
        }
        serde_json::Value::Array(items) => {
            let header = match state.env {
                Some(env) => format!("// json-pointer: {pointer} (env: {env})\n"),
                None => format!("// json-pointer: {pointer}\n"),
            };
            if header.len() + canonical(value).len() > TARGET_CHUNK_CHARS {
                let capacity = TARGET_CHUNK_CHARS.saturating_sub(header.len() + 2);
                let mut elements: Vec<String> = Vec::new();
                let mut group_size = 0usize;
                let mut first = 0usize;
                for (index, item) in items.iter().enumerate() {
                    let body = canonical(item);
                    let added = body.len() + usize::from(!elements.is_empty());
                    if group_size + added > capacity {
                        emit_array_group(state, budget, pointer, &header, &elements, first);
                        elements.clear();
                        group_size = 0;
                    }
                    if body.len() > capacity {
                        continue;
                    }
                    if elements.is_empty() {
                        first = index;
                    } else {
                        group_size += 1;
                    }
                    group_size += body.len();
                    elements.push(body);
                }
                emit_array_group(state, budget, pointer, &header, &elements, first);
            }
            for (index, child) in items.iter().enumerate() {
                add_array_groups(
                    child,
                    &format!("{pointer}/{index}"),
                    state,
                    budget,
                    depth + 1,
                    started,
                    max_time_ms,
                    cancel,
                );
                if state.cancelled {
                    return;
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ResourceBudget;
    use attic_core::FileOccurrenceId;
    use std::path::PathBuf;

    fn input_for(text: &str, filename: &str) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from(filename),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            file_type: FileType::Json,
            language_hint: Some("json".into()),
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: crate::cancellation::CancellationToken::default(),
            resource_budget: ResourceBudget::default(),
        }
    }

    #[test]
    fn source_locations_follow_escaped_pointers_and_last_duplicate_key() {
        let doc = format!(
            "{{\r\n  \"pad\":\"{}\",\r\n  \"a/b~c\": {{\"value\":\"雪\"}},\r\n  \"dup\": \"first\",\r\n  \"dup\": \"second\"\r\n}}",
            "x".repeat(TARGET_CHUNK_CHARS)
        );
        let out = JsonAnalyzer::new().analyze(input_for(&doc, "source.json"));
        assert!(!out.retrieval_units.is_empty(), "{:?}", out.diagnostics);
        for (pointer, expected, line) in [
            ("/a~1b~0c", "{\"value\":\"雪\"}", 2),
            ("/dup", "\"second\"", 4),
        ] {
            let unit = out
                .retrieval_units
                .iter()
                .find(|unit| {
                    let metadata: serde_json::Value =
                        serde_json::from_str(unit.occurrence_metadata.as_ref().unwrap()).unwrap();
                    metadata["json_pointer"] == pointer
                })
                .unwrap();
            let metadata: serde_json::Value =
                serde_json::from_str(unit.occurrence_metadata.as_ref().unwrap()).unwrap();
            let start = metadata["source_start_byte"].as_u64().unwrap() as usize;
            let end = metadata["source_end_byte"].as_u64().unwrap() as usize;
            assert_eq!(&doc[start..end], expected);
            assert_eq!(unit.span.start_line, line);
            assert_eq!(unit.span.end_line, line + 1);
            assert_eq!(metadata["position_basis"], "source");
        }
    }

    #[test]
    fn redacted_input_locations_are_marked_as_processed() {
        let text = "{\n  \"secret\": \"[REDACTED]\"\n}";
        let mut input = input_for(text, "redacted.json");
        input.content = AnalyzerContent::RedactedBytes(text.as_bytes().to_vec());
        let out = JsonAnalyzer::new().analyze(input);
        let metadata: serde_json::Value =
            serde_json::from_str(out.retrieval_units[0].occurrence_metadata.as_ref().unwrap())
                .unwrap();
        assert_eq!(metadata["position_basis"], "processed");
        assert_eq!(metadata["source_start_byte"], 0);
        assert_eq!(metadata["source_end_byte"], text.len());
        assert_eq!(out.retrieval_units[0].span.end_line, 3);
    }

    #[test]
    fn oversized_arrays_keep_bounded_groups_and_all_leaf_units() {
        let items: Vec<_> = (0..20)
            .map(|index| serde_json::json!({"index": index, "rule": "x".repeat(240)}))
            .collect();
        let text = serde_json::json!({"rules": items}).to_string();
        let mut input = input_for(&text, "groups.json");
        input.resource_budget.max_retrieval_units = 21;
        let out = JsonAnalyzer::new().analyze(input);
        assert!(
            !out.diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == diagnostic_codes::RESOURCE_EXHAUSTED),
            "{:?}",
            out.diagnostics
        );
        let mut leaves = 0;
        let mut groups = 0;
        for unit in &out.retrieval_units {
            assert!(unit.retrieval_text.len() <= TARGET_CHUNK_CHARS);
            let metadata: serde_json::Value =
                serde_json::from_str(unit.occurrence_metadata.as_ref().unwrap()).unwrap();
            if metadata["array_start"].is_number() {
                groups += 1;
                let start = metadata["array_start"].as_u64().unwrap() as usize;
                let end = metadata["array_end"].as_u64().unwrap() as usize;
                let group: Vec<serde_json::Value> =
                    serde_json::from_str(unit.canonical_text.as_ref().unwrap()).unwrap();
                assert_eq!(group, items[start..end]);
                assert_eq!(metadata["json_pointer"], "/rules");
            } else if metadata["json_pointer"]
                .as_str()
                .unwrap()
                .starts_with("/rules/")
            {
                leaves += 1;
            }
        }
        assert_eq!(leaves, items.len());
        assert_eq!(groups, 1);
        assert!(out.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == diagnostic_codes::UNIT_TRUNCATED
                && diagnostic.message.contains("complete leaf units")
        }));
    }

    #[test]
    fn nesting_beyond_recursion_budget_is_chunked_as_text_not_recursed() {
        // Oversized at every level so decomposition would otherwise recurse.
        let big = "x".repeat(TARGET_CHUNK_CHARS);
        let mut doc = format!("{{\"leaf\":\"{big}\",\"pad\":\"{big}\"}}");
        for _ in 0..6 {
            doc = format!("{{\"k\":{doc},\"pad\":\"{big}\"}}");
        }
        let mut input = input_for(&doc, "deep.json");
        input.resource_budget.max_recursion_depth = 2;
        let out = JsonAnalyzer::new().analyze(input);
        assert!(!out.retrieval_units.is_empty());
        assert!(out.diagnostics.iter().any(|d| {
            d.code == diagnostic_codes::UNIT_TRUNCATED && d.message.contains("max_recursion_depth")
        }));
        let max_ptr_depth = out
            .retrieval_units
            .iter()
            .filter_map(|u| u.occurrence_metadata.as_deref())
            .map(|m| m.matches('/').count())
            .max()
            .unwrap();
        assert!(
            max_ptr_depth <= 2,
            "recursed past the budget: {max_ptr_depth}"
        );
        // Every byte of the deep subtree is still represented.
        let total: usize = out
            .retrieval_units
            .iter()
            .map(|u| u.canonical_text.as_deref().unwrap_or_default().len())
            .sum();
        assert!(total >= doc.len() - 200);
    }

    #[test]
    fn identical_subtrees_canonicalize_identically_across_formatting() {
        let a = r#"{ "name": "x",  "nested": { "b": 2, "a": 1 } }"#;
        let b = r#"{"nested":{"a":1,"b":2},"name":"x"}"#;
        let out_a = JsonAnalyzer::new().analyze(input_for(a, "DEV-Form.json"));
        let out_b = JsonAnalyzer::new().analyze(input_for(b, "PROD-Form.json"));
        // Canonical bodies are byte-identical across environments/formatting;
        // the env label lives only in retrieval_text + occurrence metadata.
        assert_eq!(
            out_a.retrieval_units[0].canonical_text, out_b.retrieval_units[0].canonical_text,
            "canonical bodies must match for content-hash dedup"
        );
        assert_ne!(
            out_a.retrieval_units[0].retrieval_text, out_b.retrieval_units[0].retrieval_text,
            "retrieval text keeps per-occurrence env header"
        );
        let meta_a = out_a.retrieval_units[0]
            .occurrence_metadata
            .as_deref()
            .unwrap();
        assert!(meta_a.contains("\"environment\":\"DEV\""), "{meta_a}");
        assert!(meta_a.contains("\"json_pointer\":\"\""), "{meta_a}");
    }

    #[test]
    fn malformed_json_produces_error_diagnostic_and_no_units() {
        let out = JsonAnalyzer::new().analyze(input_for("{not json", "x.json"));
        assert!(out.retrieval_units.is_empty());
        let diag = out
            .diagnostics
            .iter()
            .find(|d| d.code == diagnostic_codes::MALFORMED_INPUT)
            .expect("malformed input must carry MALFORMED_INPUT diagnostic");
        // Error severity is what makes dispatch fall back to GenericAnalyzer;
        // a warning here previously meant the file vanished from the index.
        assert_eq!(diag.severity, crate::api::DiagnosticSeverity::Error);
        assert!(out.has_errors());
    }

    #[test]
    fn unit_budget_exhaustion_is_reported_exactly_once() {
        let mut obj = serde_json::Map::new();
        for i in 0..20 {
            obj.insert(
                format!("key_{i:03}"),
                serde_json::Value::String("x".repeat(100)),
            );
        }
        let text = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap();
        let mut input = input_for(&text, "data.json");
        input.resource_budget.max_retrieval_units = 3;
        let out = JsonAnalyzer::new().analyze(input);
        let hits = out
            .diagnostics
            .iter()
            .filter(|d| d.code == diagnostic_codes::RESOURCE_EXHAUSTED)
            .count();
        assert_eq!(hits, 1, "budget exhaustion reported exactly once");
        assert!(out.retrieval_units.len() <= 3);
    }

    #[test]
    fn json_pointer_addresses_and_env_labels_present() {
        let text = r#"{"properties":{"journeyName":"IS_Journey"}}"#;
        let out = JsonAnalyzer::new().analyze(input_for(text, "UAT-Code.json"));
        assert_eq!(out.retrieval_units.len(), 1);
        let t = &out.retrieval_units[0].retrieval_text;
        assert!(t.contains("json-pointer:"), "pointer header present: {t}");
        assert!(t.contains("env: UAT"), "env label present: {t}");
    }

    #[test]
    fn large_object_decomposes_into_child_subtrees() {
        let mut obj = serde_json::Map::new();
        for i in 0..50 {
            obj.insert(
                format!("key_{i:03}"),
                serde_json::Value::String("x".repeat(500)),
            );
        }
        let text = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap();
        let out = JsonAnalyzer::new().analyze(input_for(&text, "data.json"));
        assert!(
            out.retrieval_units.len() > 1,
            "oversized root must decompose into children"
        );
        for u in &out.retrieval_units {
            assert!(
                u.retrieval_text.len() <= TARGET_CHUNK_CHARS + 256,
                "unit within target+header slack: {}",
                u.retrieval_text.len()
            );
        }
    }
}
