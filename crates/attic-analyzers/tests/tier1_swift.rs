use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, diagnostic_codes, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_for_language("swift", attic_analyzers::structural::swift::analyzer());
    reg
}

fn input(code: &str, token: CancellationToken) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("sample.swift"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some("swift".to_string()),
        file_type: FileType::Other,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    }
}

#[test]
fn swift_extracts_symbols_imports_heritage_and_calls() {
    let out = dispatch(
        &registry(),
        input(
            include_str!("fixtures/sample.swift"),
            CancellationToken::new(),
        ),
    );

    assert_eq!(out.analyzer_id, "swift-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for (qname, kind, is_definition) in [
        ("Runnable", SymbolKind::Interface, true),
        ("Payload", SymbolKind::Class, true),
        ("Worker", SymbolKind::Class, true),
        ("Worker", SymbolKind::Class, false),
        ("Worker.init", SymbolKind::Method, true),
        ("Worker.run", SymbolKind::Method, true),
        ("Worker.pretty", SymbolKind::Method, true),
        ("helper", SymbolKind::Function, true),
        ("Runnable.run", SymbolKind::Method, false),
    ] {
        assert!(
            out.symbols.iter().any(|s| {
                s.qualified_name == qname && s.kind == kind && s.is_definition == is_definition
            }),
            "missing {qname} as {kind:?}; got {:?}",
            out.symbols
                .iter()
                .map(|s| (&s.qualified_name, s.kind, s.is_definition))
                .collect::<Vec<_>>()
        );
    }

    for specifier in ["Foundation", "SupportKit"] {
        assert!(
            out.imports
                .iter()
                .any(|i| i.raw_specifier == specifier && i.import_kind == "IMPORT"),
            "missing import {specifier}; got {:?}",
            out.imports
                .iter()
                .map(|i| (&i.raw_specifier, &i.import_kind))
                .collect::<Vec<_>>()
        );
    }

    for (rel_type, target) in [
        ("EXTENDS", "BaseWorker"),
        ("IMPLEMENTS", "Runnable"),
        ("EXTENDS", "Worker"),
        ("IMPLEMENTS", "CustomStringConvertible"),
    ] {
        assert!(
            out.relationships
                .iter()
                .any(|r| r.relationship_type == rel_type && r.target_qualified_name == target),
            "missing {rel_type} -> {target}; got {:?}",
            out.relationships
                .iter()
                .map(|r| (&r.relationship_type, &r.target_qualified_name))
                .collect::<Vec<_>>()
        );
    }

    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "helper"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "expected helper() call edge; got {:?}",
        out.relationships
            .iter()
            .map(|r| (&r.relationship_type, &r.target_qualified_name, r.resolution))
            .collect::<Vec<_>>()
    );
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "describe"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "expected describe() call edge; got {:?}",
        out.relationships
            .iter()
            .map(|r| (&r.relationship_type, &r.target_qualified_name, r.resolution))
            .collect::<Vec<_>>()
    );
}

#[test]
fn swift_malformed_source_and_cancellation_are_nonfatal() {
    let malformed = "import Foundation\nclass Worker: {\n  func run( {\n    helper(\n";
    let malformed_out = dispatch(&registry(), input(malformed, CancellationToken::new()));
    assert_eq!(malformed_out.analyzer_id, "swift-treesitter");
    assert!(!malformed_out.fallback_used);
    assert!(
        malformed_out
            .diagnostics
            .iter()
            .any(|d| d.code == "PARSE_ERROR"),
        "expected PARSE_ERROR diagnostics; got {:?}",
        malformed_out.diagnostics
    );

    let token = CancellationToken::new();
    token.cancel();
    let cancelled_out = dispatch(&registry(), input("func ok() {}", token));
    assert!(
        cancelled_out
            .diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostics; got {:?}",
        cancelled_out.diagnostics
    );
}
