//! End-to-end guard for the silent-truncation defect.
//!
//! Background: the analyzer could emit a retrieval unit of unbounded size (a
//! single long line became one unit), while the embedding provider truncated
//! at a fixed token count. Anything between the two limits was indexed
//! lexically but only half-embedded, with no diagnostic — invisible data loss
//! on exactly the content people care about (config/rule files whose logic
//! lives in multi-KB single-line values).
//!
//! These tests pin the contract that closed it:
//!   1. every emitted unit is bounded,
//!   2. bounding is lossless,
//!   3. the reshaping is reported.
//!
//! The fixture is deliberately format-agnostic — JSON, a log dump, and a
//! minified bundle all exercise the same path, because the fix lives in the
//! generic analyzer rather than in any per-format code.

use attic_analyzers::api::{
    AnalyzerContent, AnalyzerInput, Analyzer, ResourceBudget, diagnostic_codes,
};
use attic_analyzers::cancellation::CancellationToken;
use attic_analyzers::generic::{GenericAnalyzer, MAX_UNIT_CHARS};
use attic_core::{FileOccurrenceId, FileType};

fn analyze(text: &str) -> attic_analyzers::api::AnalyzerOutput {
    let input = AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: std::path::PathBuf::from("fixture"),
        file_type: FileType::Json,
        language_hint: None,
        content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
        size_bytes: text.len() as u64,
        is_partial_scan: false,
        resource_budget: ResourceBudget::default(),
        cancellation_token: CancellationToken::new(),
    };
    GenericAnalyzer::new().analyze(input)
}

/// Assert the three-part contract for one piece of content.
fn assert_bounded_lossless_and_reported(label: &str, text: &str) {
    let out = analyze(text);

    for (i, u) in out.retrieval_units.iter().enumerate() {
        assert!(
            u.retrieval_text.len() <= MAX_UNIT_CHARS,
            "[{label}] unit {i} is {} bytes, over the {MAX_UNIT_CHARS}-byte cap — \
             it would be silently truncated at embedding time",
            u.retrieval_text.len()
        );
    }

    // Lossless: every byte of the source survives somewhere in the units.
    // Units are joined with '\n' at chunk boundaries, so compare on content
    // with all newlines removed — that is invariant under re-chunking.
    let original: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    let indexed: String = out
        .retrieval_units
        .iter()
        .flat_map(|u| u.retrieval_text.chars())
        .filter(|c| *c != '\n' && *c != '\r')
        .collect();
    assert_eq!(
        indexed, original,
        "[{label}] content was lost or altered while splitting"
    );

    let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        codes.contains(&diagnostic_codes::UNIT_TRUNCATED),
        "[{label}] an oversized line was reshaped but not reported; got {codes:?}"
    );
}

#[test]
fn aem_style_json_rule_expressions_are_bounded_losslessly() {
    // Shape of a real AEM form-code export: ordinary JSON structure, but each
    // rule expression is a single multi-KB line.
    let mut json = String::from("{\n  \"items\": {\n");
    for r in 0..6 {
        let expr = format!(
            "value != null && value != '' && {} && predicate_{r}(input)",
            "someLongCondition && ".repeat(200)
        );
        json.push_str(&format!(
            "    \"custom:BreTwoOnPremApiCall_{r}\": {{ \"rule\": \"{expr}\" }},\n"
        ));
    }
    json.push_str("    \"_end\": true\n  }\n}\n");

    assert_bounded_lossless_and_reported("aem-json", &json);
}

#[test]
fn single_line_log_dump_is_bounded_losslessly() {
    // A log/SQL dump: one enormous line, no structure at all.
    let line = format!("2026-09-15T00:00:00Z ERROR {}", "payload=".repeat(3_000));
    assert_bounded_lossless_and_reported("log-dump", &line);
}

#[test]
fn minified_bundle_is_bounded_losslessly() {
    // Minified JS: the whole file is effectively one line.
    let bundle = format!(
        "!function(){{{}}}();",
        "var a=1,b=2,c=3;function f(){{return a+b+c}}".repeat(300)
    );
    assert_bounded_lossless_and_reported("minified-js", &bundle);
}

#[test]
fn ordinary_source_is_untouched_by_the_cap() {
    // Regression guard in the other direction: normal code must not gain
    // extra units or a spurious diagnostic just because the cap exists.
    let src = "pub fn a() -> u32 { 1 }\npub fn b() -> u32 { 2 }\n".repeat(5);
    let out = analyze(&src);

    assert_eq!(
        out.retrieval_units.len(),
        1,
        "small ordinary source must remain a single unit"
    );
    let codes: Vec<&str> = out.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(
        !codes.contains(&diagnostic_codes::UNIT_TRUNCATED),
        "no split occurred, so no truncation diagnostic is warranted; got {codes:?}"
    );
}

/// The whole point of bounding units: they must fit what the embedding layer
/// will actually read, or the fix has only moved the loss downstream.
///
/// `attic-analyzers` cannot depend on `attic-semantic`, so the gate value is
/// restated here. It must equal
/// `attic_semantic::selection::SelectionConfig::MAX_INPUT_BYTES_DEFAULT`
/// (= Qwen3 `DEFAULT_MAX_TOKENS` × `MIN_BYTES_PER_TOKEN`).
const SEMANTIC_GATE_BYTES: usize = 1_024 * 2;

#[test]
fn every_emitted_unit_fits_the_semantic_gate() {
    let mut json = String::from("{\n");
    for r in 0..4 {
        json.push_str(&format!(
            "  \"rule_{r}\": \"{}\",\n",
            "condition && ".repeat(400)
        ));
    }
    json.push_str("  \"_end\": true\n}\n");

    let out = analyze(&json);
    assert!(!out.retrieval_units.is_empty(), "must emit units");
    for (i, u) in out.retrieval_units.iter().enumerate() {
        assert!(
            u.retrieval_text.len() <= SEMANTIC_GATE_BYTES,
            "unit {i} is {} bytes, above the {SEMANTIC_GATE_BYTES}-byte semantic \
             gate — it would be excluded from embedding (EX_TOO_LARGE) or, \
             worse, admitted and tokenizer-truncated",
            u.retrieval_text.len()
        );
    }
}
