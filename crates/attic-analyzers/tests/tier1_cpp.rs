use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &str, token: CancellationToken) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("widget.cpp"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some("cpp".to_string()),
        file_type: FileType::Cpp,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    }
}

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::cpp::analyzer());
    reg
}

#[test]
fn cpp_fixture_extracts_symbols_imports_heritage_and_calls() {
    let reg = registry();
    let out = dispatch(
        &reg,
        input(
            include_str!("fixtures/widget.cpp"),
            CancellationToken::new(),
        ),
    );

    assert_eq!(out.analyzer_id, "cpp-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Module && s.qualified_name == "app.core")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Class && s.qualified_name == "app.core.Widget")
    );
    assert!(out.symbols.iter().any(|s| {
        s.kind == SymbolKind::Method && s.qualified_name == "app.core.Widget.run" && s.is_definition
    }));
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Method && s.qualified_name == "app.core.Widget.Widget")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Method && s.qualified_name == "app.core.Widget.~Widget")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Method
                && s.qualified_name == "app.core.Widget.operator()")
    );
    for (raw, kind) in [
        ("widget.hpp", "INCLUDE_QUOTE"),
        ("vector", "INCLUDE_ANGLE"),
        ("support", "USING_NAMESPACE"),
        ("util::Helper", "USING_DECLARATION"),
    ] {
        assert!(
            out.imports
                .iter()
                .any(|i| i.raw_specifier == raw && i.import_kind == kind),
            "missing import {raw} [{kind}]; got {:?}",
            out.imports
        );
    }
    let extends_targets: Vec<_> = out
        .relationships
        .iter()
        .filter(|r| r.relationship_type == "EXTENDS")
        .map(|r| r.target_qualified_name.as_str())
        .collect();
    assert!(extends_targets.contains(&"Base"));
    assert!(extends_targets.contains(&"Detail"));
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "helper"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "expected a call to helper; got {:?}",
        out.relationships
    );
}

#[test]
fn cpp_preprocessor_conditionals_mark_partial_without_fallback() {
    let reg = registry();
    let src = "#if FOO\nclass Enabled {};\n#else\nclass Disabled {};\n#endif\n";
    let out = dispatch(&reg, input(src, CancellationToken::new()));

    assert_eq!(out.analyzer_id, "cpp-treesitter");
    assert!(!out.fallback_used);
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "PREPROCESSOR_PARTIAL"),
        "diagnostics: {:?}",
        out.diagnostics
    );
    assert!(!out.structurally_complete);
    assert!(!out.symbols.is_empty());
}

#[test]
fn cpp_malformed_input_stays_specialized() {
    let reg = registry();
    let out = dispatch(
        &reg,
        input(
            "class Broken : public {\nvoid run(\n",
            CancellationToken::new(),
        ),
    );

    assert_eq!(out.analyzer_id, "cpp-treesitter");
    assert!(!out.fallback_used);
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "PARSE_ERROR"
                && d.severity != attic_analyzers::DiagnosticSeverity::Error),
        "diagnostics: {:?}",
        out.diagnostics
    );
}

#[test]
fn cpp_pre_cancelled_analysis_reports_cancelled() {
    let reg = registry();
    let token = CancellationToken::new();
    token.cancel();
    let out = dispatch(&reg, input("int main() { return 0; }\n", token));
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == attic_analyzers::diagnostic_codes::CANCELLED)
    );
}
