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
        path: PathBuf::from("sample.c"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some("c".to_string()),
        file_type: FileType::C,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    }
}

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::c::analyzer());
    reg
}

#[test]
fn c_fixture_extracts_symbols_imports_macros_and_calls() {
    let reg = registry();
    let out = dispatch(
        &reg,
        input(include_str!("fixtures/sample.c"), CancellationToken::new()),
    );

    assert_eq!(out.analyzer_id, "c-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.imports
            .iter()
            .any(|i| i.raw_specifier == "local.h" && i.import_kind == "INCLUDE_QUOTE")
    );
    assert!(
        out.imports
            .iter()
            .any(|i| i.raw_specifier == "stdio.h" && i.import_kind == "INCLUDE_ANGLE")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Macro && s.short_name == "COUNT")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::TypeAlias && s.short_name == "Item")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Class && s.short_name == "Mode")
    );
    assert!(out.symbols.iter().any(|s| {
        s.kind == SymbolKind::Function && s.qualified_name == "add" && s.is_definition
    }));
    assert!(out.symbols.iter().any(|s| {
        s.kind == SymbolKind::Function && s.qualified_name == "helper" && !s.is_definition
    }));
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Variable && s.short_name == "global")
    );
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
fn c_preprocessor_conditionals_mark_partial_without_fallback() {
    let reg = registry();
    let src = "#if ENABLE_FOO\nint enabled(void) { return 1; }\n#else\nint disabled(void) { return 0; }\n#endif\n";
    let out = dispatch(&reg, input(src, CancellationToken::new()));

    assert_eq!(out.analyzer_id, "c-treesitter");
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
fn c_malformed_input_stays_specialized() {
    let reg = registry();
    let out = dispatch(
        &reg,
        input("int broken( {\n  return 1;\n}\n", CancellationToken::new()),
    );

    assert_eq!(out.analyzer_id, "c-treesitter");
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
fn c_pre_cancelled_analysis_reports_cancelled() {
    let reg = registry();
    let token = CancellationToken::new();
    token.cancel();
    let out = dispatch(&reg, input("int value(void) { return 1; }\n", token));
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == attic_analyzers::diagnostic_codes::CANCELLED)
    );
}
