//! Tier 2 (tags.scm-based) structural coverage — one fixture per language,
//! plus the `.tsx` grammar-variant bug fix proof.
//!
//! Exercises the *public* API only (`default_registry` + `dispatch` +
//! `AnalyzerRegistry::select`) exactly as a real caller would — the internal
//! `tags_generic` table is intentionally not `pub`.

use std::path::PathBuf;

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, CancellationToken, CapabilityKind, CapabilityLevel,
    ResourceBudget, default_registry, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &str, language_hint: &str, file_type: FileType) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("fixture"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: Some(language_hint.to_string()),
        file_type,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

// ── C ────────────────────────────────────────────────────────────────────

#[test]
fn c_fixture_extracts_function_symbol() {
    let reg = default_registry();
    let code = "int add(int a, int b) {\n    return a + b;\n}\n";
    let out = dispatch(&reg, input(code, "c", FileType::C));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "add" && s.kind == SymbolKind::Function),
        "expected an 'add' Function symbol; got {:?}",
        out.symbols
    );
}

// ── C++ ──────────────────────────────────────────────────────────────────

#[test]
fn cpp_fixture_extracts_class_symbol() {
    let reg = default_registry();
    let code = "class Foo {\npublic:\n    void bar() {}\n};\n";
    let out = dispatch(&reg, input(code, "cpp", FileType::Cpp));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Foo" && s.kind == SymbolKind::Class),
        "expected a 'Foo' Class symbol; got {:?}",
        out.symbols
    );
}

// ── Ruby ─────────────────────────────────────────────────────────────────

#[test]
fn ruby_fixture_extracts_class_and_method_symbols() {
    let reg = default_registry();
    let code = "class Greeter\n  def hello(name)\n    puts name\n  end\nend\n";
    let out = dispatch(&reg, input(code, "ruby", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Greeter" && s.kind == SymbolKind::Class),
        "expected a 'Greeter' Class symbol; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "hello" && s.kind == SymbolKind::Method),
        "expected a 'hello' Method symbol; got {:?}",
        out.symbols
    );
}

// ── C# ───────────────────────────────────────────────────────────────────

#[test]
fn csharp_fixture_extracts_class_and_method_symbols() {
    let reg = default_registry();
    let code = "namespace Demo {\n  class Widget {\n    void Run() {}\n  }\n}\n";
    let out = dispatch(&reg, input(code, "csharp", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Widget" && s.kind == SymbolKind::Class),
        "expected a 'Widget' Class symbol; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Run" && s.kind == SymbolKind::Method),
        "expected a 'Run' Method symbol; got {:?}",
        out.symbols
    );
}

// ── Scala ────────────────────────────────────────────────────────────────

#[test]
fn scala_fixture_extracts_object_and_function_symbols() {
    let reg = default_registry();
    let code =
        "object Hello {\n  def main(args: Array[String]): Unit = {\n    println(\"hi\")\n  }\n}\n";
    let out = dispatch(&reg, input(code, "scala", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Hello" && s.kind == SymbolKind::Class),
        "expected a 'Hello' object mapped to Class; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "main" && s.kind == SymbolKind::Function),
        "expected a 'main' Function symbol; got {:?}",
        out.symbols
    );
}

// ── PHP ──────────────────────────────────────────────────────────────────

#[test]
fn php_fixture_extracts_class_and_method_symbols() {
    let reg = default_registry();
    let code = "<?php\nclass Widget {\n  function run() {}\n}\n";
    let out = dispatch(&reg, input(code, "php", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Widget" && s.kind == SymbolKind::Class),
        "expected a 'Widget' Class symbol; got {:?}",
        out.symbols
    );
    // PHP's own tags.scm maps `method_declaration` to `@definition.function`
    // (not `.method`) — asserting Function here is correct per that query,
    // not a limitation of this engine.
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "run" && s.kind == SymbolKind::Function),
        "expected a 'run' Function symbol; got {:?}",
        out.symbols
    );
}

// ── Swift ────────────────────────────────────────────────────────────────

#[test]
fn swift_fixture_extracts_class_and_method_symbols() {
    let reg = default_registry();
    let code = "class Greeter {\n  func greet() {}\n}\n";
    let out = dispatch(&reg, input(code, "swift", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Greeter" && s.kind == SymbolKind::Class),
        "expected a 'Greeter' Class symbol; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "greet" && s.kind == SymbolKind::Method),
        "expected a 'greet' Method symbol; got {:?}",
        out.symbols
    );
}

// ── Lua ──────────────────────────────────────────────────────────────────

#[test]
fn lua_fixture_extracts_function_symbol() {
    let reg = default_registry();
    let code = "function greet(name)\n  print(\"hi \" .. name)\nend\n";
    let out = dispatch(&reg, input(code, "lua", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "greet" && s.kind == SymbolKind::Function),
        "expected a 'greet' Function symbol; got {:?}",
        out.symbols
    );
}

// ── Rust (new tier-2 coverage; no tier-1 hand-written Rust analyzer exists) ─

#[test]
fn rust_fixture_extracts_struct_and_function_symbols() {
    let reg = default_registry();
    let code = "pub struct Point {\n    x: i32,\n    y: i32,\n}\n\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    let out = dispatch(&reg, input(code, "rust", FileType::Rust));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "Point" && s.kind == SymbolKind::Class),
        "expected a 'Point' Class symbol; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.short_name == "add" && s.kind == SymbolKind::Function),
        "expected an 'add' Function symbol; got {:?}",
        out.symbols
    );
}

// ── Dockerfile (hand-authored query; no upstream tags.scm exists) ──────────

#[test]
fn dockerfile_fixture_extracts_stage_names() {
    let reg = default_registry();
    let code = "FROM node:18 AS builder\nFROM builder AS runner\n";
    let out = dispatch(&reg, input(code, "dockerfile", FileType::Other));
    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert!(
        out.symbols.iter().any(|s| s.short_name == "builder"),
        "expected a 'builder' stage-name symbol; got {:?}",
        out.symbols
    );
    assert!(
        out.symbols.iter().any(|s| s.short_name == "runner"),
        "expected a 'runner' stage-name symbol; got {:?}",
        out.symbols
    );
}

// ── Honest capability declaration (not overclaimed) ────────────────────────

#[test]
fn tier2_capabilities_match_the_honest_table() {
    let reg = default_registry();
    let (analyzer, is_generic) = reg.select(FileType::Rust, Some("rust"));
    assert!(!is_generic);
    let caps = &analyzer.descriptor().capabilities;

    assert_eq!(
        caps.level_for(CapabilityKind::StructuralParse),
        CapabilityLevel::Full
    );
    assert_eq!(
        caps.level_for(CapabilityKind::SymbolExtraction),
        CapabilityLevel::Basic
    );
    assert_eq!(
        caps.level_for(CapabilityKind::ImportExtraction),
        CapabilityLevel::None
    );
    // Deviation from the plan's suggested table, documented in
    // `tags_generic`'s module docs: `ReferenceExtraction` is honestly `None`
    // here because no reference-related artifact is ever populated in
    // `AnalyzerOutput`, unlike the plan's original suggestion of `Basic`.
    assert_eq!(
        caps.level_for(CapabilityKind::ReferenceExtraction),
        CapabilityLevel::None
    );
    assert_eq!(
        caps.level_for(CapabilityKind::RelationshipResolution),
        CapabilityLevel::None
    );
}

// ── `.tsx` bug fix — end to end through the real registry ──────────────────

#[test]
fn tsx_file_routes_to_jsx_aware_grammar_and_parses_cleanly() {
    let reg = default_registry();
    let code = "export function Greeting(props: { name: string }) {\n  return <div className=\"greeting\">Hello, {props.name}!</div>;\n}\n";
    let out = dispatch(&reg, input(code, "tsx", FileType::TypeScript));

    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert_eq!(out.analyzer_id, "tsx-treesitter");
    assert!(
        !out.diagnostics.iter().any(|d| d.code == "PARSE_ERROR"),
        "JSX must parse cleanly under LANGUAGE_TSX (no ERROR nodes); \
         diagnostics: {:?}",
        out.diagnostics
    );
    assert!(
        out.symbols.iter().any(|s| s.short_name == "Greeting"),
        "expected a 'Greeting' function symbol; got {:?}",
        out.symbols
    );
}

/// Sibling proof: a plain `.ts` file (no JSX) must still resolve to the
/// plain TypeScript grammar, not the TSX one — the fix must not regress the
/// far more common non-JSX case.
#[test]
fn ts_file_still_routes_to_plain_typescript_grammar() {
    let reg = default_registry();
    let code = "export function add(a: number, b: number): number {\n  return a + b;\n}\n";
    let out = dispatch(&reg, input(code, "typescript", FileType::TypeScript));

    assert!(!out.fallback_used, "diagnostics: {:?}", out.diagnostics);
    assert_eq!(out.analyzer_id, "typescript-treesitter");
    assert!(out.symbols.iter().any(|s| s.short_name == "add"));
}
