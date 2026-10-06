use std::path::PathBuf;

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, CancellationToken, DiagnosticSeverity, ResourceBudget,
    default_registry, dispatch,
};
use attic_core::{FileOccurrenceId, FileType};

const TARGET_BYTES: usize = 5 * 1024 * 1024;

struct Case {
    label: &'static str,
    file_type: FileType,
    hint: &'static str,
    path: &'static str,
    expected_analyzer: &'static str,
    header: &'static str,
    filler_line: &'static str,
}

fn large_flat_source(header: &str, filler_line: &str) -> String {
    let mut body = String::with_capacity(TARGET_BYTES + filler_line.len());
    body.push_str(header);
    while body.len() < TARGET_BYTES {
        body.push_str(filler_line);
    }
    body
}

fn input(path: &str, body: String, file_type: FileType, hint: &str) -> AnalyzerInput {
    let size = body.len() as u64;
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from(path),
        content: AnalyzerContent::FullBytes(body.into_bytes()),
        language_hint: Some(hint.to_string()),
        file_type,
        size_bytes: size,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

#[test]
fn tier1_large_flat_inputs_do_not_overflow() {
    let reg = default_registry();
    let cases = [
        Case {
            label: "rust",
            file_type: FileType::Rust,
            hint: "rust",
            path: "fixture.rs",
            expected_analyzer: "rust-treesitter",
            header: "fn safe_token() {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "kotlin",
            file_type: FileType::Other,
            hint: "kotlin",
            path: "fixture.kt",
            expected_analyzer: "kotlin-treesitter",
            header: "fun safeToken() {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "scala",
            file_type: FileType::Other,
            hint: "scala",
            path: "fixture.scala",
            expected_analyzer: "scala-treesitter",
            header: "def safeToken(): Unit = {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "lua",
            file_type: FileType::Other,
            hint: "lua",
            path: "fixture.lua",
            expected_analyzer: "lua-treesitter",
            header: "local function safe_token() end\n",
            filler_line: "-- flat filler line for large-input guard\n",
        },
        Case {
            label: "ruby",
            file_type: FileType::Other,
            hint: "ruby",
            path: "fixture.rb",
            expected_analyzer: "ruby-treesitter",
            header: "def safe_token\nend\n",
            filler_line: "# flat filler line for large-input guard\n",
        },
        Case {
            label: "php",
            file_type: FileType::Other,
            hint: "php",
            path: "fixture.php",
            expected_analyzer: "php-treesitter",
            header: "<?php\nfunction safe_token() {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "swift",
            file_type: FileType::Other,
            hint: "swift",
            path: "fixture.swift",
            expected_analyzer: "swift-treesitter",
            header: "func safeToken() {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "c",
            file_type: FileType::C,
            hint: "c",
            path: "fixture.c",
            expected_analyzer: "c-treesitter",
            header: "void safe_token(void) {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "cpp",
            file_type: FileType::Cpp,
            hint: "cpp",
            path: "fixture.cpp",
            expected_analyzer: "cpp-treesitter",
            header: "void safe_token() {}\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "csharp",
            file_type: FileType::Other,
            hint: "csharp",
            path: "fixture.cs",
            expected_analyzer: "csharp-treesitter",
            header: "class C { static void SafeToken() {} }\n",
            filler_line: "// flat filler line for large-input guard\n",
        },
        Case {
            label: "dockerfile",
            file_type: FileType::Other,
            hint: "dockerfile",
            path: "Dockerfile",
            expected_analyzer: "dockerfile-treesitter",
            header: "FROM alpine\n",
            filler_line: "# flat filler line for large-input guard\n",
        },
    ];

    for case in cases {
        let out = dispatch(
            &reg,
            input(
                case.path,
                large_flat_source(case.header, case.filler_line),
                case.file_type,
                case.hint,
            ),
        );
        assert_eq!(
            out.analyzer_id, case.expected_analyzer,
            "[{}] wrong analyzer selected",
            case.label
        );
        assert!(
            !out.fallback_used,
            "[{}] must stay on the specialized analyzer",
            case.label
        );
        assert!(
            !out.retrieval_units.is_empty(),
            "[{}] lexical coverage must survive the large-input guard",
            case.label
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == "STRUCTURAL_TRUNCATED"),
            "[{}] large-input degradation must be observable; got {:?}",
            case.label,
            out.diagnostics
                .iter()
                .map(|d| (&d.code, &d.message))
                .collect::<Vec<_>>()
        );
        assert!(
            !out.diagnostics
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Error),
            "[{}] large flat input should degrade, not fail: {:?}",
            case.label,
            out.diagnostics
        );
    }
}
