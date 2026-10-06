use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken,
    DiagnosticSeverity, GenericAnalyzer, ResourceBudget, diagnostic_codes, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &str, hint: &str) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("fixture.lua"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some(hint.to_string()),
        file_type: FileType::Other,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_for_language("lua", attic_analyzers::structural::lua::analyzer());
    reg
}

#[test]
fn lua_extracts_symbols_imports_and_calls() {
    const SRC: &str = include_str!("fixtures/module.lua");
    let out = dispatch(&registry(), input(SRC, "lua"));

    assert_eq!(out.analyzer_id, "lua-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for raw in ["app.mod", "util.helpers"] {
        assert!(
            out.imports.iter().any(|i| i.raw_specifier == raw),
            "missing import {raw:?}; got {:?}",
            out.imports
                .iter()
                .map(|i| &i.raw_specifier)
                .collect::<Vec<_>>()
        );
    }

    for (qualified, kind) in [
        ("greet", SymbolKind::Function),
        ("local_helper", SymbolKind::Function),
        ("M.foo", SymbolKind::Method),
        ("M.bar", SymbolKind::Method),
    ] {
        assert!(
            out.symbols
                .iter()
                .any(|s| s.qualified_name == qualified && s.kind == kind),
            "expected {qualified} as {kind:?}; got {:?}",
            out.symbols
                .iter()
                .map(|s| (&s.qualified_name, s.kind))
                .collect::<Vec<_>>()
        );
    }

    for callee in ["greet", "local_helper", "foo"] {
        assert!(
            out.relationships.iter().any(|r| {
                r.relationship_type == "CALL"
                    && r.target_qualified_name == callee
                    && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
            }),
            "expected call to {callee}; got {:?}",
            out.relationships
        );
    }
}

#[test]
fn lua_malformed_input_stays_specialized_with_parse_diagnostic() {
    let src = "function broken(\n  local x =\n";
    let out = dispatch(&registry(), input(src, "lua"));

    assert_eq!(out.analyzer_id, "lua-treesitter");
    assert!(!out.fallback_used);
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "PARSE_ERROR" && d.severity == DiagnosticSeverity::Warning),
        "expected parse warning; got {:?}",
        out.diagnostics
    );
}

#[test]
fn lua_pre_cancelled_analysis_reports_cancelled() {
    let reg = registry();
    let token = CancellationToken::new();
    token.cancel();
    let src = "function ok() end\n";
    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("cancelled.lua"),
            content: AnalyzerContent::FullBytes(src.as_bytes().to_vec()),
            language_hint: Some(String::from("lua")),
            file_type: FileType::Other,
            size_bytes: src.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        },
    );

    assert_eq!(out.analyzer_id, "lua-treesitter");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostic; got {:?}",
        out.diagnostics
    );
}
