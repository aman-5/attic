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
        path: PathBuf::from("fixture.kt"),
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
    reg.register_for_language("kotlin", attic_analyzers::structural::kotlin::analyzer());
    reg
}

#[test]
fn kotlin_extracts_symbols_imports_heritage_and_calls() {
    const SRC: &str = include_str!("fixtures/orders.kt");
    let out = dispatch(&registry(), input(SRC, "kotlin"));

    assert_eq!(out.analyzer_id, "kotlin-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for raw in [
        "com.acme.base.BaseController",
        "com.acme.base.RunnableSupport",
        "com.acme.shared.SupportService",
        "org.springframework.web.bind.annotation.GetMapping",
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
        ("com.acme.orders.OrderController", SymbolKind::Class),
        ("com.acme.orders.OrderController.find", SymbolKind::Method),
        (
            "com.acme.orders.OrderController.service",
            SymbolKind::Variable,
        ),
        (
            "com.acme.orders.OrderController.support",
            SymbolKind::Variable,
        ),
        (
            "com.acme.orders.OrderController.enabled",
            SymbolKind::Variable,
        ),
        ("com.acme.orders.OrderController.Paths", SymbolKind::Class),
        (
            "com.acme.orders.OrderController.Paths.BASE",
            SymbolKind::Constant,
        ),
        ("com.acme.orders.OrderService", SymbolKind::Interface),
        ("com.acme.orders.OrderMetrics", SymbolKind::Class),
        ("com.acme.orders.OrderId", SymbolKind::TypeAlias),
        ("com.acme.orders.total", SymbolKind::Function),
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

    let interface_method = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "com.acme.orders.OrderService.find")
        .expect("interface method");
    assert!(
        !interface_method.is_definition,
        "interface signatures are declarations without bodies"
    );

    assert!(
        out.relationships.iter().any(
            |r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "BaseController"
        )
    );
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "IMPLEMENTS"
            && r.target_qualified_name == "Runnable"
            && r.resolution == attic_analyzers::ResolutionLevel::Syntactic
    }));
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "find"
                && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
        }),
        "expected local call edge; got {:?}",
        out.relationships
    );
}

#[test]
fn kotlin_malformed_input_stays_specialized_with_parse_diagnostic() {
    let src = "package demo\nclass Broken( {\n  fun nope(:\n";
    let out = dispatch(&registry(), input(src, "kotlin"));

    assert_eq!(out.analyzer_id, "kotlin-treesitter");
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
fn kotlin_pre_cancelled_analysis_reports_cancelled() {
    let reg = registry();
    let token = CancellationToken::new();
    token.cancel();
    let src = "package demo\nclass Cancelled\n";
    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("cancelled.kt"),
            content: AnalyzerContent::FullBytes(src.as_bytes().to_vec()),
            language_hint: Some(String::from("kotlin")),
            file_type: FileType::Other,
            size_bytes: src.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        },
    );

    assert_eq!(out.analyzer_id, "kotlin-treesitter");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostic; got {:?}",
        out.diagnostics
    );
}
