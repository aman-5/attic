use std::path::PathBuf;

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, CancellationToken, DiagnosticSeverity, ResourceBudget,
    default_registry, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &'static str, file_type: FileType, language_hint: Option<&str>) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("fixture"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: language_hint.map(str::to_string),
        file_type,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

fn input_with_token(
    code: &'static str,
    file_type: FileType,
    language_hint: Option<&str>,
    token: CancellationToken,
) -> AnalyzerInput {
    AnalyzerInput {
        cancellation_token: token,
        ..input(code, file_type, language_hint)
    }
}

#[test]
fn csharp_symbols_imports_heritage_and_calls_are_extracted() {
    const SRC: &str = include_str!("fixtures/SampleApp.cs");
    let out = dispatch(
        &default_registry(),
        input(SRC, FileType::Other, Some("csharp")),
    );

    assert_eq!(out.analyzer_id, "csharp-treesitter");
    for q in [
        "Demo.Services.IWorker",
        "Demo.Services.BaseWorker",
        "Demo.Services.AuditRecord",
        "Demo.Services.Job",
        "Demo.Services.Worker",
        "Demo.Services.Worker.Run",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == q),
            "missing {q}; got {:?}",
            out.symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }
    assert!(
        out.imports
            .iter()
            .any(|i| i.import_kind == "STATIC" && i.raw_specifier == "Demo.Helpers.MathHelpers")
    );
    assert!(
        out.imports
            .iter()
            .any(|i| i.import_kind == "ALIAS" && i.raw_specifier == "Demo.Models.Widget")
    );
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "BaseWorker")
    );
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "IMPLEMENTS"
            && r.target_qualified_name == "IWorker"
            && r.resolution == attic_analyzers::ResolutionLevel::Syntactic
    }));
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "CALL"
            && r.target_qualified_name == "Common"
            && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
    }));
    let property = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "Demo.Services.Worker.Value")
        .expect("property");
    assert_eq!(property.kind, SymbolKind::Variable);
}

#[test]
fn rust_symbols_imports_heritage_and_calls_are_extracted() {
    const SRC: &str = include_str!("fixtures/sample.rs");
    let out = dispatch(
        &default_registry(),
        input(SRC, FileType::Rust, Some("rust")),
    );

    assert_eq!(out.analyzer_id, "rust-treesitter");
    for q in [
        "tools.Runner",
        "tools.Worker",
        "tools.RunnerId",
        "tools.MAX_RETRIES",
        "tools.helper",
        "tools.Runner.status",
        "tools.Runner.run",
        "nested.boot",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == q),
            "missing {q}; got {:?}",
            out.symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Macro && s.short_name == "log_value")
    );
    for raw in [
        "support",
        "crate::tools::helper",
        "crate::tools::Runner",
        "crate::tools::Worker",
        "self::tools::RunnerId",
        "super::tools::helper",
    ] {
        assert!(
            out.imports.iter().any(|i| i.raw_specifier == raw),
            "missing import {raw}; got {:?}",
            out.imports
                .iter()
                .map(|i| &i.raw_specifier)
                .collect::<Vec<_>>()
        );
    }
    assert!(
        out.relationships
            .iter()
            .any(|r| { r.relationship_type == "EXTENDS" && r.target_qualified_name == "Named" })
    );
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "IMPLEMENTS" && r.target_qualified_name == "Worker"
        })
    );
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "CALL"
            && r.target_qualified_name == "helper"
            && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
    }));
}

#[test]
fn dockerfile_stages_imports_and_edges_are_extracted() {
    const SRC: &str = include_str!("fixtures/Sample.Dockerfile");
    let out = dispatch(
        &default_registry(),
        input(SRC, FileType::Other, Some("dockerfile")),
    );

    assert_eq!(out.analyzer_id, "dockerfile-treesitter");
    for stage in ["builder", "runner"] {
        assert!(
            out.symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Module && s.short_name == stage),
            "missing stage {stage}; got {:?}",
            out.symbols
        );
    }
    for var in ["BASE_IMAGE", "APP_HOME", "PATH"] {
        assert!(
            out.symbols.iter().any(|s| s.short_name == var),
            "missing build variable {var}"
        );
    }
    for raw in ["src/app.sh", "assets/config.json", "assets/onbuild.txt"] {
        assert!(
            out.imports.iter().any(|i| i.raw_specifier == raw),
            "missing import {raw}; got {:?}",
            out.imports
                .iter()
                .map(|i| &i.raw_specifier)
                .collect::<Vec<_>>()
        );
    }
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "EXTENDS"
            && r.target_qualified_name == "builder"
            && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
    }));
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "REFERENCES"
            && r.target_qualified_name == "builder"
            && r.resolution == attic_analyzers::ResolutionLevel::SymbolResolved
    }));
    assert!(
        !out.relationships
            .iter()
            .any(|r| r.relationship_type == "CALL"),
        "dockerfile analyzer must not fabricate call edges"
    );
}

#[test]
fn new_tier1_languages_remain_specialized_on_malformed_input() {
    let reg = default_registry();
    let cases = [
        (
            FileType::Other,
            Some("csharp"),
            "namespace Demo { class Broken { void Run( { new Widget( }",
            "csharp-treesitter",
        ),
        (
            FileType::Rust,
            Some("rust"),
            "pub trait Broken: { fn run(&self) \n impl Broken for {",
            "rust-treesitter",
        ),
        (
            FileType::Other,
            Some("dockerfile"),
            "FROM alpine AS\nCOPY --from= builder\nONBUILD COPY [",
            "dockerfile-treesitter",
        ),
    ];
    for (file_type, hint, src, analyzer_id) in cases {
        let out = dispatch(&reg, input(src, file_type, hint));
        assert_eq!(out.analyzer_id, analyzer_id);
        assert!(
            !out.fallback_used,
            "[{analyzer_id}] malformed parse must stay specialized"
        );
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == "PARSE_ERROR" && d.severity != DiagnosticSeverity::Error),
            "[{analyzer_id}] expected non-fatal parse diagnostic; got {:?}",
            out.diagnostics
        );
    }
}

#[test]
fn cancellation_is_honored_before_extraction_starts() {
    let reg = default_registry();
    for (file_type, hint, src, analyzer_id) in [
        (
            FileType::Other,
            Some("csharp"),
            include_str!("fixtures/SampleApp.cs"),
            "csharp-treesitter",
        ),
        (
            FileType::Rust,
            Some("rust"),
            include_str!("fixtures/sample.rs"),
            "rust-treesitter",
        ),
        (
            FileType::Other,
            Some("dockerfile"),
            include_str!("fixtures/Sample.Dockerfile"),
            "dockerfile-treesitter",
        ),
    ] {
        let token = CancellationToken::new();
        token.cancel();
        let out = dispatch(&reg, input_with_token(src, file_type, hint, token));
        assert_eq!(out.analyzer_id, analyzer_id);
        assert!(
            out.diagnostics.iter().any(|d| d.code == "CANCELLED"),
            "[{analyzer_id}] cancellation must be observable; got {:?}",
            out.diagnostics
        );
    }
}
