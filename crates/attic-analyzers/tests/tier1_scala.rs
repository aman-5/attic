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
        path: PathBuf::from("fixture.scala"),
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
    reg.register_for_language("scala", attic_analyzers::structural::scala::analyzer());
    reg
}

#[test]
fn scala_extracts_symbols_imports_heritage_and_calls() {
    const SRC: &str = include_str!("fixtures/inventory.scala");
    let out = dispatch(&registry(), input(SRC, "scala"));

    assert_eq!(out.analyzer_id, "scala-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for raw in [
        "com.acme.base.BaseTrait",
        "com.acme.base.RunSupport",
        "scala.collection.mutable.*",
    ] {
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
        ("com.acme.BaseTrait", SymbolKind::Interface),
        ("com.acme.Runnable", SymbolKind::Interface),
        ("com.acme.Child", SymbolKind::Class),
        ("com.acme.Child.run", SymbolKind::Method),
        ("com.acme.Child.helper", SymbolKind::Method),
        ("com.acme.Child.SIZE", SymbolKind::Constant),
        ("com.acme.Child.Alias", SymbolKind::TypeAlias),
        ("com.acme.Hello", SymbolKind::Module),
        ("com.acme.Hello.greet", SymbolKind::Method),
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

    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "BaseTrait")
    );
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "IMPLEMENTS" && r.target_qualified_name == "Runnable")
    );
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "helper"
                && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
        }),
        "expected intra-file helper call; got {:?}",
        out.relationships
    );
}

#[test]
fn scala_malformed_input_stays_specialized_with_parse_diagnostic() {
    let src = "package demo\nclass Broken extends {\n  def nope(x: Int =\n";
    let out = dispatch(&registry(), input(src, "scala"));

    assert_eq!(out.analyzer_id, "scala-treesitter");
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
fn scala_pre_cancelled_analysis_reports_cancelled() {
    let reg = registry();
    let token = CancellationToken::new();
    token.cancel();
    let src = "package demo\nobject Cancelled\n";
    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("cancelled.scala"),
            content: AnalyzerContent::FullBytes(src.as_bytes().to_vec()),
            language_hint: Some(String::from("scala")),
            file_type: FileType::Other,
            size_bytes: src.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        },
    );

    assert_eq!(out.analyzer_id, "scala-treesitter");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostic; got {:?}",
        out.diagnostics
    );
}
