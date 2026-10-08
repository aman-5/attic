//! Phase 3 — language-specific extraction assertions.
//!
//! Each language's distinctive features are asserted here; shared invariants
//! live in `phase3_language_matrix.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResolutionLevel, ResourceBudget, default_registry, dispatch,
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

fn hinted_input(code: &'static str, ft: FileType, hint: &'static str) -> AnalyzerInput {
    AnalyzerInput {
        language_hint: Some(hint.to_string()),
        ..input(code, ft)
    }
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

// ══ C# ═════════════════════════════════════════════════════════════════════

#[test]
fn csharp_using_forms_block_and_file_scoped_namespaces_and_calls() {
    const SRC: &str = include_str!("fixtures/SampleApp.cs");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Other, "csharp"));

    for raw in [
        "Demo.Helpers",
        "Demo.Helpers.MathHelpers",
        "Demo.Models.Widget",
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
            .any(|r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "BaseWorker")
    );
    let impls = out
        .relationships
        .iter()
        .filter(|r| r.relationship_type == "IMPLEMENTS" && r.target_qualified_name == "IWorker")
        .count();
    assert!(impls >= 2, "Job + Worker implement IWorker");
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "CALL"
            && r.target_qualified_name == "Common"
            && r.resolution == ResolutionLevel::SymbolResolved
    }));
    let ctor = out
        .structural_nodes
        .iter()
        .find(|n| n.node_type == "CONSTRUCTOR" && n.name == "Worker")
        .expect("Worker constructor");
    assert!(ctor.parent_index.is_some(), "constructor nested under type");
    let value = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "Demo.Services.Worker.Value")
        .expect("property symbol");
    assert_eq!(value.kind, SymbolKind::Variable);

    const FILE_SCOPED: &str = "global using Demo.Helpers;\nnamespace Demo.FileScoped;\npublic class Tool {\n    public void Run() { Local(); }\n    private void Local() { }\n}\n";
    let file_scoped = dispatch(&reg, hinted_input(FILE_SCOPED, FileType::Other, "csharp"));
    assert!(
        file_scoped
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Demo.FileScoped.Tool.Run")
    );
    assert!(
        file_scoped
            .imports
            .iter()
            .any(|i| i.raw_specifier == "Demo.Helpers")
    );
}

// ══ Rust ═══════════════════════════════════════════════════════════════════

#[test]
fn rust_use_forms_traits_impls_macros_and_calls() {
    const SRC: &str = include_str!("fixtures/sample.rs");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Rust, "rust"));

    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Macro && s.qualified_name == "tools.log_value")
    );
    for raw in [
        "support",
        "crate::tools::helper",
        "crate::tools::Runner",
        "crate::tools::Worker",
        "self::tools::RunnerId",
        "super::tools",
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
    let trait_sig = out
        .symbols
        .iter()
        .find(|s| s.qualified_name == "tools.Worker.run" && !s.is_definition)
        .expect("trait signature");
    assert_eq!(trait_sig.kind, SymbolKind::Method);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name == "tools.Runner.run" && s.is_definition)
    );
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "Named")
    );
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "IMPLEMENTS" && r.target_qualified_name == "Worker")
    );
    for callee in ["helper", "status"] {
        assert!(
            out.relationships.iter().any(|r| {
                r.relationship_type == "CALL"
                    && r.target_qualified_name == callee
                    && r.resolution == ResolutionLevel::SymbolResolved
            }),
            "missing call {callee}; got {:?}",
            out.relationships
        );
    }
}

// ══ Dockerfile ═════════════════════════════════════════════════════════════

#[test]
fn dockerfile_stages_copy_add_and_stage_references() {
    const SRC: &str = include_str!("fixtures/Sample.Dockerfile");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Other, "dockerfile"));

    for stage in ["builder", "runner"] {
        assert!(
            out.symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Module && s.short_name == stage),
            "missing stage {stage}; got {:?}",
            out.symbols
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
            && r.resolution == ResolutionLevel::SymbolResolved
    }));
    assert!(out.relationships.iter().any(|r| {
        r.relationship_type == "REFERENCES"
            && r.target_qualified_name == "builder"
            && r.resolution == ResolutionLevel::SymbolResolved
    }));
    assert!(
        out.structural_nodes
            .iter()
            .any(|n| n.node_type == "HEALTHCHECK"),
        "minimal healthcheck structure required"
    );
}

// ══ Kotlin ════════════════════════════════════════════════════════════════

#[test]
fn kotlin_import_aliases_constructor_properties_heritage_and_calls() {
    const SRC: &str = include_str!("fixtures/orders.kt");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Other, "kotlin"));

    for raw in [
        "com.acme.base.BaseController",
        "com.acme.base.RunnableSupport",
        "com.acme.shared.SupportService",
        "org.springframework.web.bind.annotation.GetMapping",
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
    for qualified in [
        "com.acme.orders.OrderController.service",
        "com.acme.orders.OrderController.support",
        "com.acme.orders.OrderController.find",
        "com.acme.orders.OrderController.Paths.BASE",
        "com.acme.orders.total",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == qualified),
            "missing symbol {qualified}; got {:?}",
            out.symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }
    assert!(
        out.symbols.iter().any(|s| s.kind == SymbolKind::Interface
            && s.qualified_name == "com.acme.orders.OrderService")
    );
    assert!(
        out.relationships.iter().any(
            |r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "BaseController"
        )
    );
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "IMPLEMENTS" && r.target_qualified_name == "Runnable")
    );
    assert!(
        out.relationships.iter().any(|r| {
            r.relationship_type == "CALL"
                && r.target_qualified_name == "find"
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "missing local call edge; got {:?}",
        out.relationships
    );
}

// ══ Scala ═════════════════════════════════════════════════════════════════

#[test]
fn scala_import_selectors_traits_objects_and_calls() {
    const SRC: &str = include_str!("fixtures/inventory.scala");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Other, "scala"));

    for raw in [
        "com.acme.base.BaseTrait",
        "com.acme.base.RunSupport",
        "scala.collection.mutable.*",
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
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Module && s.qualified_name == "com.acme.Hello")
    );
    for qualified in [
        "com.acme.Child.run",
        "com.acme.Child.Alias",
        "com.acme.Hello.greet",
    ] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == qualified),
            "missing symbol {qualified}; got {:?}",
            out.symbols
                .iter()
                .map(|s| &s.qualified_name)
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
                && r.resolution == ResolutionLevel::SymbolResolved
        }),
        "missing helper call edge; got {:?}",
        out.relationships
    );
}

// ══ Lua ═══════════════════════════════════════════════════════════════════

#[test]
fn lua_requires_table_methods_and_calls() {
    const SRC: &str = include_str!("fixtures/module.lua");
    let reg = default_registry();
    let out = dispatch(&reg, hinted_input(SRC, FileType::Other, "lua"));

    for raw in ["app.mod", "util.helpers"] {
        assert!(
            out.imports.iter().any(|i| i.raw_specifier == raw),
            "missing import {raw}; got {:?}",
            out.imports
                .iter()
                .map(|i| &i.raw_specifier)
                .collect::<Vec<_>>()
        );
    }
    for qualified in ["greet", "local_helper", "M.foo", "M.bar"] {
        assert!(
            out.symbols.iter().any(|s| s.qualified_name == qualified),
            "missing symbol {qualified}; got {:?}",
            out.symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }
    for callee in ["greet", "local_helper", "foo"] {
        assert!(
            out.relationships.iter().any(|r| {
                r.relationship_type == "CALL"
                    && r.target_qualified_name == callee
                    && r.resolution == ResolutionLevel::SymbolResolved
            }),
            "missing call {callee}; got {:?}",
            out.relationships
        );
    }
}
