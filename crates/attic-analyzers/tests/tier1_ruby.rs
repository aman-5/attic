use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, diagnostic_codes, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn registry() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_for_language("ruby", attic_analyzers::structural::ruby::analyzer());
    reg
}

fn input(code: &str, token: CancellationToken) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("sample.rb"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some("ruby".to_string()),
        file_type: FileType::Other,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    }
}

#[test]
fn ruby_extracts_symbols_imports_heritage_and_calls() {
    let out = dispatch(
        &registry(),
        input(include_str!("fixtures/sample.rb"), CancellationToken::new()),
    );

    assert_eq!(out.analyzer_id, "ruby-treesitter");
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);

    for (qname, kind) in [
        ("Services", SymbolKind::Module),
        ("Services.Greeter", SymbolKind::Class),
        ("Services.Greeter.VERSION", SymbolKind::Constant),
        ("Services.Greeter.build", SymbolKind::Method),
        ("Services.Greeter.initialize", SymbolKind::Method),
        ("Services.Greeter.render", SymbolKind::Method),
    ] {
        assert!(
            out.symbols
                .iter()
                .any(|s| s.qualified_name == qname && s.kind == kind),
            "missing {qname} as {kind:?}; got {:?}",
            out.symbols
                .iter()
                .map(|s| (&s.qualified_name, s.kind))
                .collect::<Vec<_>>()
        );
    }

    for (specifier, kind) in [
        ("net/http", "REQUIRE"),
        ("./support/helper", "REQUIRE_RELATIVE"),
        ("config/boot.rb", "LOAD"),
        ("widgets/builder", "AUTOLOAD"),
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
        ("EXTENDS", "BaseGreeter"),
        ("IMPLEMENTS", "Formatters"),
        ("IMPLEMENTS", "Helpers"),
        ("IMPLEMENTS", "Hooks"),
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
                && r.target_qualified_name == "render"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "expected local render() call edge; got {:?}",
        out.relationships
            .iter()
            .map(|r| (&r.relationship_type, &r.target_qualified_name, r.resolution))
            .collect::<Vec<_>>()
    );
}

#[test]
fn ruby_malformed_source_and_cancellation_are_nonfatal() {
    let malformed = "module Broken\n  class Greeter <\n    def run(\n      puts(\n";
    let malformed_out = dispatch(&registry(), input(malformed, CancellationToken::new()));
    assert_eq!(malformed_out.analyzer_id, "ruby-treesitter");
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
    let cancelled_out = dispatch(&registry(), input("def greet() end", token));
    assert!(
        cancelled_out
            .diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "expected CANCELLED diagnostics; got {:?}",
        cancelled_out.diagnostics
    );
}
