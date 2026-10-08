use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, diagnostic_codes, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_for_language("php", attic_analyzers::structural::php::analyzer());
    reg
}

fn input(code: &str, token: CancellationToken) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("sample.php"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some("php".to_string()),
        file_type: FileType::Other,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    }
}

#[test]
fn php_extracts_symbols_imports_heritage_and_calls() {
    let out = dispatch(
        &registry(),
        input(
            include_str!("fixtures/sample.php"),
            CancellationToken::new(),
        ),
    );

    assert_eq!(out.analyzer_id, "php-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for (qname, kind, is_definition) in [
        ("App.Services", SymbolKind::Module, true),
        ("App.Services.Decorates", SymbolKind::Interface, true),
        ("App.Services.Runnable", SymbolKind::Interface, true),
        ("App.Services.Status", SymbolKind::Class, true),
        ("App.Services.helper", SymbolKind::Function, true),
        ("App.Services.Worker", SymbolKind::Class, true),
        ("App.Services.Worker.run", SymbolKind::Method, true),
        ("App.Services.Runnable.run", SymbolKind::Method, false),
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

    for (specifier, kind) in [
        ("Vendor\\Package\\BaseWorker", "USE"),
        ("Vendor\\Package\\Formatter", "USE"),
        ("Vendor\\Package\\LoggerTrait", "USE"),
        ("../bootstrap.php", "REQUIRE_ONCE"),
        ("helpers.php", "INCLUDE"),
    ] {
        assert!(
            out.imports
                .iter()
                .any(|i| i.raw_specifier == specifier && i.import_kind == kind),
            "missing {kind} import {specifier}; got {:?}",
            out.imports
                .iter()
                .map(|i| (&i.raw_specifier, &i.import_kind))
                .collect::<Vec<_>>()
        );
    }

    for (rel_type, target) in [
        ("EXTENDS", "BaseWorker"),
        ("IMPLEMENTS", "Runnable"),
        ("IMPLEMENTS", "Decorates"),
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
                && r.target_qualified_name == "decorate"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "expected $this->decorate() call edge; got {:?}",
        out.relationships
            .iter()
            .map(|r| (&r.relationship_type, &r.target_qualified_name, r.resolution))
            .collect::<Vec<_>>()
    );
}

#[test]
fn php_malformed_source_and_cancellation_are_nonfatal() {
    let malformed =
        "<?php\nnamespace Broken;\nclass Worker extends {\n  public function run(: void {\n";
    let malformed_out = dispatch(&registry(), input(malformed, CancellationToken::new()));
    assert_eq!(malformed_out.analyzer_id, "php-treesitter");
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
    let cancelled_out = dispatch(&registry(), input("<?php function ok() {}", token));
    assert!(
        cancelled_out
            .diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostics; got {:?}",
        cancelled_out.diagnostics
    );
}
