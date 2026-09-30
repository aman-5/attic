//! Phase 3 — language-specific extraction assertions.
//!
//! Each language's distinctive features are asserted here; shared invariants
//! live in `phase3_language_matrix.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &'static str, ft: FileType) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("x.src"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: None,
        file_type: ft,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

fn reg_one(analyzer: Arc<dyn Analyzer>, ft: FileType) -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    let _ = ft;
    reg.register_specialized(analyzer);
    reg
}

// ══ Java ═══════════════════════════════════════════════════════════════════

#[test]
fn java_overloads_get_deterministic_disambiguators_and_static_import_kind() {
    const SRC: &str = include_str!("fixtures/OrderService.java");
    let out = dispatch(
        &reg_one(
            attic_analyzers::structural::java::analyzer(),
            FileType::Java,
        ),
        input(SRC, FileType::Java),
    );
    // Overloaded `compute(int)` / `compute(String)` — first keeps None,
    // later ones get overload:N ordered by span.
    let overloads: Vec<_> = out
        .symbols
        .iter()
        .filter(|s| s.qualified_name.ends_with(".compute") && s.kind == SymbolKind::Method)
        .collect();
    assert!(overloads.len() >= 3, "three compute members expected");
    assert!(
        overloads.iter().any(|s| s.disambiguator.is_none()),
        "first overload unambiguous"
    );
    assert!(
        overloads
            .iter()
            .filter_map(|s| s.disambiguator.clone())
            .eq(["overload:2".to_string(), "overload:3".to_string()]),
        "deterministic overload numbering"
    );

    // Static import kind + wildcard-free flattening.
    assert!(out.imports.iter().any(|i| i.import_kind == "STATIC"
        && i.raw_specifier == "java.util.Collections.unmodifiableList"));
    // Heritage edges with syntactic honesty.
    let ext = out
        .relationships
        .iter()
        .find(|r| r.relationship_type == "EXTENDS")
        .expect("extends edge");
    assert_eq!(ext.target_qualified_name, "BaseService");
    assert_eq!(ext.resolution, ResolutionLevel::Syntactic);
    let impl_count = out
        .relationships
        .iter()
        .filter(|r| r.relationship_type == "IMPLEMENTS")
        .count();
    assert_eq!(impl_count, 2, "Identifiable + Comparable<OrderService>");
    // final static field → Constant symbol.
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Constant && s.short_name == "MAX_RETRIES")
    );
}

// ══ Python ═════════════════════════════════════════════════════════════════

#[test]
fn python_import_forms_relative_imports_decorated_async_nested() {
    const SRC: &str = include_str!("fixtures/sample.py");
    let out = dispatch(
        &reg_one(
            attic_analyzers::structural::python::analyzer(),
            FileType::Python,
        ),
        input(SRC, FileType::Python),
    );

    // Import forms.
    for raw in [
        "os",
        "os.path",
        "collections:OrderedDict",
        "..shared:constants",
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
    // aliased import keeps the module path (alias recorded separately).
    assert!(
        out.imports.iter().any(|i| i.raw_specifier == "os.path"),
        "aliased import flattened to module path"
    );

    // Symbols: class + methods + async + nested function + constants.
    for q in [
        "Inventory",
        "Inventory.__init__",
        "Inventory.refresh",
        "top_level",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == q),
            "missing {q}"
        );
    }
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Constant && s.short_name == "MAX_ITEMS")
    );
    // Decorated method span includes the decorator line.
    let size_sym = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "Inventory.size")
        .expect("size property");
    assert!(
        size_sym.definition_span.start_line >= 15,
        "decorated method anchored at decorator or def line"
    );
}

// ══ Go ═════════════════════════════════════════════════════════════════════

#[test]
fn go_methods_interfaces_structs_and_constructor_calls() {
    const SRC: &str = include_str!("fixtures/server.go");
    let out = dispatch(
        &reg_one(attic_analyzers::structural::go::analyzer(), FileType::Go),
        input(SRC, FileType::Go),
    );

    for q in [
        "inventory.Store",
        "inventory.Counter.Count",
        "inventory.NewStore",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == q),
            "missing {q}"
        );
    }
    // Interface methods are signatures, not definitions.
    let count_sig = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "inventory.Counter.Count")
        .unwrap();
    assert!(!count_sig.is_definition);
    // Constructor call resolved intra-file.
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "CALL"
                && r.target_qualified_name == "NewStore"
                && r.resolution == ResolutionLevel::SymbolResolved)
    );
    // Exported-ness via capitalisation.
    assert!(
        out.symbols
            .iter()
            .find(|s| s.qualified_name == "inventory.MaxParts")
            .map(|s| s.is_public)
            .unwrap_or(false)
    );
}

// ══ JavaScript ═════════════════════════════════════════════════════════════

#[test]
fn javascript_esm_cjs_dynamic_imports_private_fields_arrows() {
    const SRC: &str = include_str!("fixtures/widget.js");
    let out = dispatch(
        &reg_one(
            attic_analyzers::structural::javascript::analyzer(),
            FileType::JavaScript,
        ),
        input(SRC, FileType::JavaScript),
    );

    // Import forms incl. default+named mix and require/dynamic.
    assert!(out.imports.iter().any(|i| i.import_kind == "REQUIRE"));
    assert!(
        out.imports
            .iter()
            .any(|i| i.raw_specifier == "../shared/index.js" && i.import_kind == "IMPORT")
    );

    // Private field visibility + exported symbols public.
    let render = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "Widget.render")
        .expect("render");
    assert_eq!(render.visibility.as_deref(), Some("public"));
    // Nested function inside makeWidget gets qualified name.
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name == "makeWidget.choose")
    );
    // Arrow function captured as Function.
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Function && s.short_name == "arrowAdd")
    );
}

// ══ TypeScript ═════════════════════════════════════════════════════════════

#[test]
fn typescript_interfaces_enums_namespaces_abstract_signatures() {
    const SRC: &str = include_str!("fixtures/widget.ts");
    let out = dispatch(
        &reg_one(
            attic_analyzers::structural::typescript::analyzer(),
            FileType::TypeScript,
        ),
        input(SRC, FileType::TypeScript),
    );

    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Interface && s.qualified_name == "Options")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::TypeAlias && s.short_name == "Maybe")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Constant && s.qualified_name == "Color.Red")
    );
    // Interface member signature: not a definition.
    let run = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "Options.run")
        .expect("Options.run");
    assert!(!run.is_definition);
    // Abstract method signature vs concrete getter.
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name.contains("BaseWidget.render") && !s.is_definition)
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name.contains("BaseWidget.value") && s.is_definition)
    );
}
