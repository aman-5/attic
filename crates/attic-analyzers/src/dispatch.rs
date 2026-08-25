//! Dispatch — route analysis requests through the registry with panic recovery.
//!
//! ## Algorithm
//!
//! 1. Select the best analyzer from the registry for the input's `FileType`.
//! 2. If the selected analyzer **is** the generic fallback, run it directly
//!    (no panic-catching overhead needed — `GenericAnalyzer` is the terminal
//!    safe fallback).
//! 3. For specialized analyzers:
//!    - Wrap the call in `std::panic::catch_unwind`.
//!    - **Success, no error diagnostics** → return output as-is.
//!    - **Success, error diagnostics** → run `GenericAnalyzer` on the
//!      pre-cloned content (available for `FullBytes`/`RedactedBytes`);
//!      append original error diagnostics + `FALLBACK_USED` for traceability.
//!    - **Panic** → run `GenericAnalyzer` on a pre-cloned copy of the content
//!      (available for `FullBytes`/`RedactedBytes`); annotate with
//!      `PANIC_CAUGHT` + `FALLBACK_USED`.
//!
//! ## Streaming inputs
//!
//! `StreamingHandle` content is **collected once** into `RedactedBytes` before
//! splitting, so both specialized and fallback paths receive a replayable copy.
//! Collection uses `attic_discovery::secrets::collect_all`, which respects the
//! Phase 1B redaction boundary (no raw file path is ever reopened).
//!
//! If collection fails (I/O error), `fallback_input` is `None` and a minimal
//! output is returned on failure.

use std::panic::{self, AssertUnwindSafe};

use attic_core::FileOccurrenceId;
use attic_discovery::secrets;

use crate::api::{
    Analyzer, AnalyzerContent, AnalyzerDiagnostic, AnalyzerInput, AnalyzerOutput,
    CapabilityKind, DiagnosticSeverity, diagnostic_codes,
};
use crate::generic::GenericAnalyzer;
use crate::registry::AnalyzerRegistry;

// ─────────────────────────────────────────────────────────────────────────────
// Public entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Dispatch `input` through `registry`, with panic recovery for specialized
/// analyzers.
///
/// See module-level documentation for the full algorithm.
pub fn dispatch(registry: &AnalyzerRegistry, input: AnalyzerInput) -> AnalyzerOutput {
    // `FileType` is `Copy`; read it before consuming `input`.
    let file_type = input.file_type;
    let (analyzer, is_generic) = registry.select(file_type);

    // GenericAnalyzer is the terminal safe fallback — no panic catch needed.
    if is_generic {
        return analyzer.analyze(input);
    }

    // Clone bytes content before passing ownership to the specialized analyzer.
    // For streaming inputs, collect to RedactedBytes so fallback replay is possible.
    let (fallback_input, specialized_input) = split_for_fallback(input);

    // Wrap the specialized call in catch_unwind.
    // SAFETY: we assert unwind safety because:
    // - `AnalyzerInput` holds no lock guards or other !UnwindSafe state that
    //   would leave shared state corrupt after a panic.
    // - The `Arc<dyn Analyzer>` is read-only during `analyze()`.
    let result = panic::catch_unwind(AssertUnwindSafe(|| analyzer.analyze(specialized_input)));

    match result {
        // ── Happy path ──────────────────────────────────────────────────────
        Ok(output) if !has_errors(&output) => output,

        // ── Specialized returned error diagnostics ──────────────────────────
        // Run GenericAnalyzer on the pre-cloned content so the caller always
        // receives useful retrieval units. Original error diagnostics are
        // preserved for traceability.
        Ok(specialized_output) => match fallback_input {
            Some(fb_input) => {
                let generic = GenericAnalyzer::new();
                let mut fb_output = generic.analyze(fb_input);
                // Preserve the original error diagnostics for traceability.
                fb_output.diagnostics.extend(specialized_output.diagnostics);
                fb_output.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::FALLBACK_USED,
                    "Specialized analyzer produced error diagnostics; \
                     output produced by GenericAnalyzer fallback.",
                ));
                fb_output.fallback_used = true;
                fb_output
            }
            None => {
                // Streaming collection already happened in split_for_fallback;
                // reaching None here means the collection itself failed.
                minimal_error_output(
                    "Specialized analyzer produced errors; \
                     streaming content collection failed — no fallback replay possible.",
                )
            }
        },

        // ── Specialized panicked ─────────────────────────────────────────────
        Err(_panic_payload) => match fallback_input {
            Some(fb_input) => {
                // Re-run on the pre-cloned content with the generic analyzer.
                let generic = GenericAnalyzer::new();
                let mut output = generic.analyze(fb_input);
                output.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::PANIC_CAUGHT,
                    "Specialized analyzer panicked; recovered via GenericAnalyzer fallback.",
                ));
                output.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::FALLBACK_USED,
                    "Output produced by GenericAnalyzer after specialized analyzer panic.",
                ));
                output.fallback_used = true;
                output
            }
            None => {
                // StreamingHandle collection failed; cannot replay.
                minimal_panic_output(
                    "Specialized analyzer panicked on StreamingHandle input; \
                     no fallback replay is possible.",
                )
            }
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` if `output` contains at least one `Error`-severity diagnostic.
fn has_errors(output: &AnalyzerOutput) -> bool {
    output
        .diagnostics
        .iter()
        .any(|d| d.severity == DiagnosticSeverity::Error)
}

/// Split `input` into `(fallback_input, specialized_input)`.
///
/// - `FullBytes`/`RedactedBytes`: the bytes are cloned so both inputs carry
///   the same content.
/// - `StreamingHandle`: the stream is collected once into `RedactedBytes` via
///   `secrets::collect_all`, preserving the Phase 1B redaction boundary.
///   Both inputs receive the same collected bytes.  If collection fails,
///   `fallback_input` is `None`.
fn split_for_fallback(input: AnalyzerInput) -> (Option<AnalyzerInput>, AnalyzerInput) {
    // Destructure to avoid partial-move issues.
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

    // Build fallback content (clone for bytes; collect-then-clone for streaming).
    let (fallback_content_opt, specialized_content): (Option<AnalyzerContent>, AnalyzerContent) =
        match content {
            AnalyzerContent::FullBytes(ref bytes) => (
                Some(AnalyzerContent::FullBytes(bytes.clone())),
                content,
            ),
            AnalyzerContent::RedactedBytes(ref bytes) => (
                Some(AnalyzerContent::RedactedBytes(bytes.clone())),
                content,
            ),
            AnalyzerContent::StreamingHandle(mut stream) => {
                // Collect the already-redacted stream bytes so both paths can
                // replay the content. This never reopens the raw file.
                match secrets::collect_all(&mut stream) {
                    Ok(scan_result) => {
                        let collected = AnalyzerContent::RedactedBytes(
                            scan_result.redacted.into_bytes(),
                        );
                        let collected_copy = match &collected {
                            AnalyzerContent::RedactedBytes(b) => {
                                AnalyzerContent::RedactedBytes(b.clone())
                            }
                            _ => unreachable!(),
                        };
                        (Some(collected_copy), collected)
                    }
                    Err(_) => {
                        // Collection failed; no fallback possible.
                        // Provide an empty RedactedBytes to the specialized path
                        // (it will fail gracefully) and None for fallback.
                        (None, AnalyzerContent::RedactedBytes(vec![]))
                    }
                }
            }
        };

    // Specialized input uses the (possibly collected) content.
    let specialized = AnalyzerInput {
        file_occurrence_id,
        path: path.clone(),
        content: specialized_content,
        language_hint: language_hint.clone(),
        file_type,
        size_bytes,
        is_partial_scan,
        cancellation_token: cancellation_token.clone(),
        resource_budget: resource_budget.clone(),
    };

    // Fallback input uses the cloned bytes (or None if collection failed).
    let fallback = fallback_content_opt.map(|fc| AnalyzerInput {
        file_occurrence_id,
        path,
        content: fc,
        language_hint,
        file_type,
        size_bytes,
        is_partial_scan,
        cancellation_token,
        resource_budget,
    });

    (fallback, specialized)
}

/// Build a minimal `AnalyzerOutput` for use when a panic occurs on a
/// streaming input (no fallback analysis possible).
fn minimal_panic_output(message: &str) -> AnalyzerOutput {
    AnalyzerOutput {
        analyzer_id: "dispatch".to_string(),
        analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
        file_occurrence_id: FileOccurrenceId::new_v4(),
        structural_nodes: vec![],
        symbols: vec![],
        imports: vec![],
        relationships: vec![],
        retrieval_units: vec![],
        diagnostics: vec![
            AnalyzerDiagnostic::warning(diagnostic_codes::PANIC_CAUGHT, message),
            AnalyzerDiagnostic::warning(
                diagnostic_codes::FALLBACK_USED,
                "No fallback available for StreamingHandle after panic.",
            ),
        ],
        fallback_used: true,
        capability_used: CapabilityKind::Lexical,
    }
}

/// Build a minimal `AnalyzerOutput` for use when a specialized analyzer
/// produced errors but no fallback replay is possible (streaming collection
/// failed).
fn minimal_error_output(message: &str) -> AnalyzerOutput {
    AnalyzerOutput {
        analyzer_id: "dispatch".to_string(),
        analyzer_version: env!("CARGO_PKG_VERSION").to_string(),
        file_occurrence_id: FileOccurrenceId::new_v4(),
        structural_nodes: vec![],
        symbols: vec![],
        imports: vec![],
        relationships: vec![],
        retrieval_units: vec![],
        diagnostics: vec![
            AnalyzerDiagnostic::warning(diagnostic_codes::FALLBACK_USED, message),
        ],
        fallback_used: true,
        capability_used: CapabilityKind::Lexical,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use attic_core::{FileOccurrenceId, FileType};

    use crate::api::{
        AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
        AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, ResourceBudget,
        diagnostic_codes,
    };
    use crate::cancellation::CancellationToken;
    use crate::generic::GenericAnalyzer;
    use crate::registry::AnalyzerRegistry;

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn make_text_input(text: &str, file_type: FileType) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("test.txt"),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: None,
            file_type,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        }
    }

    fn generic_registry() -> AnalyzerRegistry {
        AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>)
    }

    // ── Stub: succeeds cleanly ───────────────────────────────────────────────

    struct SuccessStub {
        desc: AnalyzerDescriptor,
    }

    impl SuccessStub {
        fn new(file_type: FileType) -> Self {
            Self {
                desc: AnalyzerDescriptor {
                    name: "success-stub".to_string(),
                    version: "0.1.0".to_string(),
                    description: "always succeeds cleanly".to_string(),
                    supported_file_types: vec![file_type],
                    capabilities: AnalyzerCapabilities::single(
                        CapabilityKind::Lexical,
                        CapabilityLevel::Full,
                    ),
                },
            }
        }
    }

    impl Analyzer for SuccessStub {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.desc
        }

        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            AnalyzerOutput {
                analyzer_id: "success-stub".to_string(),
                analyzer_version: "0.1.0".to_string(),
                file_occurrence_id: input.file_occurrence_id,
                structural_nodes: vec![],
                symbols: vec![],
                imports: vec![],
                relationships: vec![],
                retrieval_units: vec![],
                diagnostics: vec![],
                fallback_used: false,
                capability_used: CapabilityKind::Lexical,
            }
        }
    }

    // ── Stub: returns an Error-severity diagnostic ────────────────────────────

    struct ErrorStub {
        desc: AnalyzerDescriptor,
    }

    impl ErrorStub {
        fn new(file_type: FileType) -> Self {
            Self {
                desc: AnalyzerDescriptor {
                    name: "error-stub".to_string(),
                    version: "0.1.0".to_string(),
                    description: "always emits an error diagnostic".to_string(),
                    supported_file_types: vec![file_type],
                    capabilities: AnalyzerCapabilities::single(
                        CapabilityKind::Lexical,
                        CapabilityLevel::Full,
                    ),
                },
            }
        }
    }

    impl Analyzer for ErrorStub {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.desc
        }

        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            AnalyzerOutput {
                analyzer_id: "error-stub".to_string(),
                analyzer_version: "0.1.0".to_string(),
                file_occurrence_id: input.file_occurrence_id,
                structural_nodes: vec![],
                symbols: vec![],
                imports: vec![],
                relationships: vec![],
                retrieval_units: vec![],
                diagnostics: vec![AnalyzerDiagnostic::error(
                    "TEST_ERROR",
                    "synthetic error from ErrorStub",
                )],
                fallback_used: false,
                capability_used: CapabilityKind::Lexical,
            }
        }
    }

    // ── Stub: panics unconditionally ──────────────────────────────────────────

    struct PanicStub {
        desc: AnalyzerDescriptor,
    }

    impl PanicStub {
        fn new(file_type: FileType) -> Self {
            Self {
                desc: AnalyzerDescriptor {
                    name: "panic-stub".to_string(),
                    version: "0.1.0".to_string(),
                    description: "always panics".to_string(),
                    supported_file_types: vec![file_type],
                    capabilities: AnalyzerCapabilities::single(
                        CapabilityKind::Lexical,
                        CapabilityLevel::Full,
                    ),
                },
            }
        }
    }

    impl Analyzer for PanicStub {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.desc
        }

        fn analyze(&self, _input: AnalyzerInput) -> AnalyzerOutput {
            panic!("deliberate panic from PanicStub");
        }
    }

    // ── Build a registry with a single specialized stub ───────────────────────

    fn registry_with_specialized(analyzer: Arc<dyn Analyzer>) -> AnalyzerRegistry {
        let mut reg = generic_registry();
        reg.register_specialized(analyzer);
        reg
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// When only the generic registry is used, dispatch calls generic directly.
    #[test]
    fn dispatch_with_generic_registry_calls_generic_directly() {
        let registry = generic_registry();
        let input = make_text_input("hello\nworld\n", FileType::Rust);
        let output = dispatch(&registry, input);

        // Generic produces retrieval units, no panic diagnostics.
        assert!(!output.retrieval_units.is_empty(), "generic must produce units");
        assert!(!output.fallback_used, "generic path must not set fallback_used");
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            !codes.contains(&diagnostic_codes::PANIC_CAUGHT),
            "generic path must not emit PANIC_CAUGHT"
        );
        assert!(
            !codes.contains(&diagnostic_codes::FALLBACK_USED),
            "generic path must not emit FALLBACK_USED"
        );
    }

    /// Specialized analyzer that succeeds returns its output unchanged.
    #[test]
    fn dispatch_specialized_success_returns_output() {
        let stub = Arc::new(SuccessStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);
        let input = make_text_input("fn main() {}", FileType::Rust);

        let output = dispatch(&registry, input);

        assert_eq!(output.analyzer_id, "success-stub");
        assert!(!output.fallback_used);
        assert!(output.diagnostics.is_empty(), "success path must have no diagnostics");
    }

    /// Specialized analyzer that emits error diagnostics causes GenericAnalyzer
    /// fallback to run. Output must have retrieval units, FALLBACK_USED, and
    /// the original error diagnostic preserved.
    #[test]
    fn dispatch_specialized_fatal_error_runs_generic_fallback_and_produces_units() {
        let stub = Arc::new(ErrorStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);
        // Use text that GenericAnalyzer can produce units from.
        let input = make_text_input("fn main() {\n    println!(\"hello\");\n}\n", FileType::Rust);

        let output = dispatch(&registry, input);

        // Must have run GenericAnalyzer — output comes from the generic analyzer.
        assert_eq!(
            output.analyzer_id, "generic",
            "error fallback must produce output from GenericAnalyzer; got: {}",
            output.analyzer_id
        );
        assert!(output.fallback_used, "error path must set fallback_used");

        // Must have produced retrieval units from GenericAnalyzer.
        assert!(
            !output.retrieval_units.is_empty(),
            "error fallback must produce retrieval units via GenericAnalyzer; got 0 units"
        );

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::FALLBACK_USED),
            "error path must emit FALLBACK_USED; got: {codes:?}"
        );
        // Original error diagnostic must still be present for traceability.
        assert!(
            codes.contains(&"TEST_ERROR"),
            "original error diagnostic must be preserved; got: {codes:?}"
        );
    }

    /// Specialized analyzer that panics gets PANIC_CAUGHT + FALLBACK_USED,
    /// and GenericAnalyzer output is returned.
    #[test]
    fn dispatch_specialized_panic_caught_adds_panic_caught_and_fallback_used() {
        let stub = Arc::new(PanicStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);
        // Use text that GenericAnalyzer can produce units from.
        let input = make_text_input("fn main() {\n    println!(\"hello\");\n}\n", FileType::Rust);

        let output = dispatch(&registry, input);

        // Must have PANIC_CAUGHT and FALLBACK_USED.
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::PANIC_CAUGHT),
            "panic path must emit PANIC_CAUGHT; got: {codes:?}"
        );
        assert!(
            codes.contains(&diagnostic_codes::FALLBACK_USED),
            "panic path must emit FALLBACK_USED; got: {codes:?}"
        );
        assert!(output.fallback_used, "panic path must set fallback_used = true");

        // GenericAnalyzer must have produced at least one retrieval unit.
        assert!(
            !output.retrieval_units.is_empty(),
            "panic fallback must produce retrieval units via GenericAnalyzer"
        );
    }

    /// GenericAnalyzer is always a safe terminal fallback: even if called
    /// as a specialized analyzer, it never panics on valid text.
    #[test]
    fn generic_analyzer_is_terminal_safe_fallback() {
        // Register GenericAnalyzer as the specialized analyzer for Rust too.
        // This simulates using generic as a workhorse.
        let generic = Arc::new(GenericAnalyzer::new());
        let registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);
        // Note: GenericAnalyzer has empty supported_file_types, so register_specialized
        // will skip it.  Instead verify that dispatch with generic-only registry
        // never panics across multiple content types.
        drop(registry);

        let registry = generic_registry();

        // FullBytes
        let out1 = dispatch(&registry, make_text_input("line1\nline2\n", FileType::Rust));
        assert!(!out1.retrieval_units.is_empty());

        // Empty file
        let out2 = dispatch(&registry, make_text_input("", FileType::Text));
        assert!(out2.retrieval_units.is_empty());

        // Invalid UTF-8 via FullBytes
        let bad_input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("bad.bin"),
            content: AnalyzerContent::FullBytes(vec![0xFF, 0xFE, b'h', b'i']),
            language_hint: None,
            file_type: FileType::Other,
            size_bytes: 4,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };
        let out3 = dispatch(&registry, bad_input);
        let codes: Vec<&str> = out3.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::MALFORMED_INPUT),
            "invalid UTF-8 must emit MALFORMED_INPUT; got: {codes:?}"
        );
        // Must not panic — the test itself is the proof.
    }
}
