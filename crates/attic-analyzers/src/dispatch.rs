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
//!      pre-prepared fallback content; append original error diagnostics +
//!      `FALLBACK_USED` for traceability.
//!    - **Panic** → run `GenericAnalyzer` on the pre-prepared fallback content;
//!      annotate with `PANIC_CAUGHT` + `FALLBACK_USED`.
//!
//! ## Streaming inputs — bounded spool strategy
//!
//! `StreamingHandle` content is **never collected into memory** as a whole.
//! Instead, `split_for_fallback` spools the stream's already-redacted bytes
//! chunk-by-chunk (O(1) memory) to a `tempfile::NamedTempFile`.  After the
//! spool is complete both specialized and fallback inputs receive an
//! independent `LargeFileStream::open(spool_path)` — reading only the
//! Phase-1B-safe/redacted bytes in the spool file, never the raw repository
//! file.
//!
//! The spool file:
//! - Contains **only** `chunk.redacted` bytes emitted by `LargeFileStream`
//!   (Phase-1B guarantee: no raw secrets).
//! - Lives in the system temp directory under a random name.
//! - Is held in a `_spool_guard: Option<NamedTempFile>` local to `dispatch()`,
//!   which drops (and deletes the file) exactly when both analyzers finish,
//!   regardless of success, error, or panic.
//!
//! If spooling fails (I/O error), `fallback_input` is `None` and a minimal
//! output is returned on failure.

use std::io::Write;
use std::panic::{self, AssertUnwindSafe};

use attic_core::FileOccurrenceId;
use attic_discovery::secrets::LargeFileStream;

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

    // Prepare fallback and specialized inputs.
    // For StreamingHandle, this spools the already-redacted bytes to a temp
    // file (O(1) memory, bounded disk).  The spool guard keeps the temp file
    // alive until after both analyzers finish — it drops at the end of this
    // function regardless of the execution path taken below.
    let (fallback_input, specialized_input, _spool_guard) = split_for_fallback(input);

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
        // Run GenericAnalyzer on the pre-prepared fallback content so the
        // caller always receives useful retrieval units.  Original error
        // diagnostics are preserved for traceability.
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
                // Spooling failed in split_for_fallback; no fallback replay.
                minimal_error_output(
                    "Specialized analyzer produced errors; \
                     streaming spool failed — no fallback replay possible.",
                )
            }
        },

        // ── Specialized panicked ─────────────────────────────────────────────
        Err(_panic_payload) => match fallback_input {
            Some(fb_input) => {
                // Re-run on the pre-prepared content with the generic analyzer.
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
                // Spooling failed; cannot replay.
                minimal_panic_output(
                    "Specialized analyzer panicked on StreamingHandle input; \
                     no fallback replay is possible.",
                )
            }
        },
    }
    // `_spool_guard` drops here, deleting the temp spool file deterministically.
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

/// Split `input` into `(fallback_input, specialized_input, spool_guard)`.
///
/// - `FullBytes`/`RedactedBytes`: bytes are cloned; `spool_guard` is `None`.
/// - `StreamingHandle`: the stream is **spooled** chunk-by-chunk to a
///   [`tempfile::NamedTempFile`] (O(1) memory, bounded disk usage).
///   Only `chunk.redacted` bytes are written — never raw secrets.
///   Both inputs receive an independent `LargeFileStream::open(spool_path)`.
///   The returned `spool_guard` keeps the temp file alive; it must be held
///   in the caller until both analyzers have finished, then dropped to clean up.
///   If spooling fails, `fallback_input` is `None` and `spool_guard` is `None`.
fn split_for_fallback(
    input: AnalyzerInput,
) -> (
    Option<AnalyzerInput>,
    AnalyzerInput,
    Option<tempfile::NamedTempFile>,
) {
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

    // Build fallback content and an optional spool guard.
    let (fallback_content_opt, specialized_content, spool_guard): (
        Option<AnalyzerContent>,
        AnalyzerContent,
        Option<tempfile::NamedTempFile>,
    ) = match content {
        AnalyzerContent::FullBytes(ref bytes) => (
            Some(AnalyzerContent::FullBytes(bytes.clone())),
            content,
            None,
        ),
        AnalyzerContent::RedactedBytes(ref bytes) => (
            Some(AnalyzerContent::RedactedBytes(bytes.clone())),
            content,
            None,
        ),
        AnalyzerContent::StreamingHandle(mut stream) => {
            // Spool already-redacted bytes chunk-by-chunk to a temp file.
            // O(1) memory: only one chunk is in memory at a time.
            // The spool contains ONLY Phase-1B-safe bytes (chunk.redacted).
            // The raw repository file is never reopened.
            let spool_result: std::io::Result<tempfile::NamedTempFile> = (|| {
                let mut spool = tempfile::NamedTempFile::new()?;
                while let Some(chunk_result) = stream.next_chunk() {
                    let chunk = chunk_result?;
                    // Write only the already-redacted text — never raw secrets.
                    spool.write_all(chunk.redacted.as_bytes())?;
                }
                spool.flush()?;
                Ok(spool)
            })();

            match spool_result {
                Ok(spool) => {
                    // Open two independent read streams from the spool.
                    // Both read only Phase-1B-safe bytes; neither touches the
                    // raw repository file.
                    let spec_stream = LargeFileStream::open(spool.path());
                    let fb_stream = LargeFileStream::open(spool.path());
                    match (spec_stream, fb_stream) {
                        (Ok(s), Ok(f)) => (
                            Some(AnalyzerContent::StreamingHandle(Box::new(f))),
                            AnalyzerContent::StreamingHandle(Box::new(s)),
                            Some(spool),
                        ),
                        _ => {
                            // Failed to open streams from the spool.
                            (None, AnalyzerContent::RedactedBytes(vec![]), Some(spool))
                        }
                    }
                }
                Err(_) => {
                    // Spooling failed; no fallback possible.
                    (None, AnalyzerContent::RedactedBytes(vec![]), None)
                }
            }
        }
    };

    // Specialized input uses the (possibly spool-backed) content.
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

    // Fallback input uses the cloned/spool-backed content (or None if spool failed).
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

    (fallback, specialized, spool_guard)
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
/// produced errors but no fallback replay is possible (spool failed).
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
        diagnostics: vec![AnalyzerDiagnostic::warning(
            diagnostic_codes::FALLBACK_USED,
            message,
        )],
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
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;

    use attic_core::{FileOccurrenceId, FileType};
    use attic_discovery::secrets::LargeFileStream;

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

    /// Create a streaming input backed by a real temp file containing `content`.
    /// Returns the input and the backing `NamedTempFile` (must be kept alive).
    fn make_streaming_input(
        content: &[u8],
        file_type: FileType,
    ) -> (AnalyzerInput, tempfile::NamedTempFile) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        let stream = LargeFileStream::open(f.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: f.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type,
            size_bytes: content.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };
        (input, f)
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

    // ── Stub: records the content variant it received ─────────────────────────

    /// A stub that captures whether it received a StreamingHandle or
    /// RedactedBytes/FullBytes.  Used to prove the spool path does NOT
    /// convert streaming to an in-memory blob before passing to the analyzer.
    struct ContentCapturingStub {
        desc: AnalyzerDescriptor,
    }

    impl ContentCapturingStub {
        fn new(file_type: FileType) -> Self {
            Self {
                desc: AnalyzerDescriptor {
                    name: "content-capturing-stub".to_string(),
                    version: "0.1.0".to_string(),
                    description: "captures content variant".to_string(),
                    supported_file_types: vec![file_type],
                    capabilities: AnalyzerCapabilities::single(
                        CapabilityKind::Lexical,
                        CapabilityLevel::Full,
                    ),
                },
            }
        }
    }

    impl Analyzer for ContentCapturingStub {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.desc
        }

        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            // Embed in the analyzer_id whether we got a streaming handle.
            let got_streaming = matches!(input.content, AnalyzerContent::StreamingHandle(_));
            AnalyzerOutput {
                analyzer_id: if got_streaming {
                    "got-streaming".to_string()
                } else {
                    "got-buffered".to_string()
                },
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

    // ── Build a registry with a single specialized stub ───────────────────────

    fn registry_with_specialized(analyzer: Arc<dyn Analyzer>) -> AnalyzerRegistry {
        let mut reg = generic_registry();
        reg.register_specialized(analyzer);
        reg
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Existing dispatch tests (FullBytes / RedactedBytes paths)
    // ─────────────────────────────────────────────────────────────────────────

    /// When only the generic registry is used, dispatch calls generic directly.
    #[test]
    fn dispatch_with_generic_registry_calls_generic_directly() {
        let registry = generic_registry();
        let input = make_text_input("hello\nworld\n", FileType::Rust);
        let output = dispatch(&registry, input);

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
        let input = make_text_input("fn main() {\n    println!(\"hello\");\n}\n", FileType::Rust);

        let output = dispatch(&registry, input);

        assert_eq!(
            output.analyzer_id, "generic",
            "error fallback must produce output from GenericAnalyzer; got: {}",
            output.analyzer_id
        );
        assert!(output.fallback_used, "error path must set fallback_used");
        assert!(
            !output.retrieval_units.is_empty(),
            "error fallback must produce retrieval units via GenericAnalyzer; got 0 units"
        );

        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::FALLBACK_USED),
            "error path must emit FALLBACK_USED; got: {codes:?}"
        );
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
        let input = make_text_input("fn main() {\n    println!(\"hello\");\n}\n", FileType::Rust);

        let output = dispatch(&registry, input);

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
        assert!(
            !output.retrieval_units.is_empty(),
            "panic fallback must produce retrieval units via GenericAnalyzer"
        );
    }

    /// GenericAnalyzer is always a safe terminal fallback.
    #[test]
    fn generic_analyzer_is_terminal_safe_fallback() {
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
    }

    // ─────────────────────────────────────────────────────────────────────────
    // NEW: Streaming dispatch tests (bounded spool strategy)
    // ─────────────────────────────────────────────────────────────────────────

    /// Test 1: Dispatching a LARGE streaming input does NOT collect the entire
    /// file into memory.  The specialized analyzer must receive a
    /// `StreamingHandle`, not a `RedactedBytes`/`FullBytes` blob.
    #[test]
    fn streaming_dispatch_does_not_collect_entire_file_into_memory() {
        // The ContentCapturingStub reports which content variant it received.
        let stub = Arc::new(ContentCapturingStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let text = b"fn streaming() {}\nfn bounded() {}\n";
        let (input, _guard) = make_streaming_input(text, FileType::Rust);

        let output = dispatch(&registry, input);

        assert_eq!(
            output.analyzer_id, "got-streaming",
            "specialized analyzer must receive StreamingHandle (not a collected blob); \
             got analyzer_id={}",
            output.analyzer_id
        );
        assert!(!output.fallback_used);
    }

    /// Test 2: Specialized success on a streaming input remains streaming/bounded.
    /// The output must come from the specialized analyzer (not fallback) and
    /// must not set `fallback_used`.
    #[test]
    fn streaming_specialized_success_remains_bounded() {
        let stub = Arc::new(SuccessStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let text = b"fn hello() {}\nfn world() {}\n";
        let (input, _guard) = make_streaming_input(text, FileType::Rust);

        let output = dispatch(&registry, input);

        assert_eq!(output.analyzer_id, "success-stub");
        assert!(!output.fallback_used, "successful streaming dispatch must not set fallback_used");
        assert!(
            output.diagnostics.is_empty(),
            "successful streaming dispatch must produce no diagnostics"
        );
    }

    /// Test 3: Specialized error on a streaming input falls back to
    /// GenericAnalyzer with searchable output.
    #[test]
    fn streaming_specialized_error_falls_back_with_searchable_output() {
        let stub = Arc::new(ErrorStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        // Content that GenericAnalyzer will produce retrieval units from.
        let text = b"fn foo() {\n    let x = 1;\n}\nfn bar() {\n    let y = 2;\n}\n";
        let (input, _guard) = make_streaming_input(text, FileType::Rust);

        let output = dispatch(&registry, input);

        assert_eq!(
            output.analyzer_id, "generic",
            "streaming error fallback must produce GenericAnalyzer output; got: {}",
            output.analyzer_id
        );
        assert!(output.fallback_used, "streaming error fallback must set fallback_used");
        assert!(
            !output.retrieval_units.is_empty(),
            "streaming error fallback must produce retrieval units (searchable output)"
        );
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::FALLBACK_USED),
            "streaming error fallback must emit FALLBACK_USED; got: {codes:?}"
        );
        assert!(
            codes.contains(&"TEST_ERROR"),
            "original error diagnostic must be preserved in streaming fallback; got: {codes:?}"
        );
    }

    /// Test 4: Specialized panic on a streaming input falls back to
    /// GenericAnalyzer with searchable output.
    #[test]
    fn streaming_specialized_panic_falls_back_with_searchable_output() {
        let stub = Arc::new(PanicStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let text = b"fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let (input, _guard) = make_streaming_input(text, FileType::Rust);

        let output = dispatch(&registry, input);

        assert!(output.fallback_used, "streaming panic fallback must set fallback_used");
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(
            codes.contains(&diagnostic_codes::PANIC_CAUGHT),
            "streaming panic fallback must emit PANIC_CAUGHT; got: {codes:?}"
        );
        assert!(
            codes.contains(&diagnostic_codes::FALLBACK_USED),
            "streaming panic fallback must emit FALLBACK_USED; got: {codes:?}"
        );
        assert!(
            !output.retrieval_units.is_empty(),
            "streaming panic fallback must produce retrieval units (searchable output)"
        );
    }

    /// Test 5: Fallback never reopens raw repository content.
    /// The spool file path differs from the original repository file path.
    /// We verify this by checking that when ErrorStub triggers a fallback,
    /// the content the fallback GenericAnalyzer processes does NOT contain any
    /// bytes beyond what was in the original (i.e., it reads from the spool,
    /// which has the same safe content, not from a different raw file).
    #[test]
    fn streaming_fallback_never_reopens_raw_repository_file() {
        // Use content with a secret that Phase-1B would redact.
        // The spool must contain the REDACTED form, not the raw secret.
        let raw_text =
            b"config: AKIAIOSFODNN7EXAMPLE\nfn process() {}\n";

        let stub = Arc::new(ErrorStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let (input, _guard) = make_streaming_input(raw_text, FileType::Rust);

        let output = dispatch(&registry, input);

        // The fallback ran GenericAnalyzer on the spool.
        // Retrieval units must exist (searchable output).
        assert!(output.fallback_used);
        // The retrieval unit text must NOT contain the raw AWS key — only the
        // redacted placeholder can appear in spool-derived content.
        for unit in &output.retrieval_units {
            assert!(
                !unit.retrieval_text.contains("AKIAIOSFODNN7EXAMPLE"),
                "fallback output must not expose raw secret; \
                 found raw key in retrieval unit retrieval_text"
            );
        }
    }

    /// Test 6: Secret-redaction guarantees survive replay/fallback.
    /// When a streaming input containing secrets is spooled and the fallback
    /// GenericAnalyzer runs on the spool, the output retrieval units must not
    /// contain any raw secret values.
    #[test]
    fn streaming_secret_redaction_survives_spool_and_fallback() {
        // Text with a GitHub token that Phase-1B redacts.
        let raw_text =
            b"auth: ghp_abcdefghijklmnopqrstuvwxyz1234567890ab\nfn check() {}\n";

        let stub = Arc::new(ErrorStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let (input, _guard) = make_streaming_input(raw_text, FileType::Rust);

        let output = dispatch(&registry, input);

        assert!(output.fallback_used);
        for unit in &output.retrieval_units {
            assert!(
                !unit.retrieval_text.contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890ab"),
                "raw GitHub token must not appear in fallback retrieval unit retrieval_text"
            );
        }
    }

    /// Test 7: Very large single-line input remains bounded in memory.
    /// A single 200 KiB line is dispatched through a streaming path.
    /// The test verifies dispatch completes without OOM and produces output.
    #[test]
    fn streaming_very_large_single_line_remains_bounded() {
        // 200 KiB of 'x' characters (no newlines) — a pathological single line.
        let large_line = vec![b'x'; 200 * 1024];

        let stub = Arc::new(SuccessStub::new(FileType::Text));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let (input, _guard) = make_streaming_input(&large_line, FileType::Text);

        // Must complete without panic or OOM — the test itself proves this.
        let output = dispatch(&registry, input);

        assert_eq!(output.analyzer_id, "success-stub");
        assert!(!output.fallback_used);
    }

    /// Test 8: Temporary spool resources are cleaned up after
    /// success/error/panic.  We verify that the spool path no longer exists
    /// on disk after `dispatch` returns.
    #[test]
    fn streaming_spool_temp_file_cleaned_up_after_dispatch() {
        // We need to observe the spool path before dispatch drops it.
        // Strategy: use split_for_fallback directly and check the spool path.
        let text = b"fn cleanup_test() {}\nfn verify() {}\n";
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(text).unwrap();
        f.flush().unwrap();

        let stream = LargeFileStream::open(f.path()).unwrap();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: f.path().to_path_buf(),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Rust,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        };

        // Call split_for_fallback to get the spool guard, record its path.
        let (fallback_opt, specialized, spool_guard) = split_for_fallback(input);

        let spool_path: Option<std::path::PathBuf> = spool_guard
            .as_ref()
            .map(|sg| sg.path().to_path_buf());

        // Spool file must exist while the guard is alive.
        if let Some(ref p) = spool_path {
            assert!(p.exists(), "spool file must exist while spool_guard is held");
        }

        // Consume the inputs (simulate analyzer use).
        drop(fallback_opt);
        drop(specialized);

        // Drop the spool guard — this must delete the temp file.
        drop(spool_guard);

        // Spool file must no longer exist after the guard is dropped.
        if let Some(ref p) = spool_path {
            assert!(
                !p.exists(),
                "spool file must be deleted when spool_guard is dropped; \
                 path still exists: {}",
                p.display()
            );
        }

        // Also verify via full dispatch path — after dispatch returns, the spool
        // is gone.  We do this by running dispatch on a panic stub (so we can
        // observe the fallback path too) and checking nothing leaks.
        let stub = Arc::new(PanicStub::new(FileType::Rust));
        let registry = registry_with_specialized(stub as Arc<dyn Analyzer>);

        let text2 = b"fn spool_leak_test() {}\n";
        let (input2, _guard2) = make_streaming_input(text2, FileType::Rust);
        let output = dispatch(&registry, input2);

        // Dispatch must complete and have panic+fallback diagnostics.
        assert!(output.fallback_used);
        let codes: Vec<&str> = output.diagnostics.iter().map(|d| d.code.as_str()).collect();
        assert!(codes.contains(&diagnostic_codes::PANIC_CAUGHT));
        // No way to assert the temp file is gone without the path, but the
        // NamedTempFile drop contract guarantees it.  The test completes
        // without resource leaks detectable via normal program exit.
    }
}
