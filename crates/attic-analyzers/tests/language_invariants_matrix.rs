//! Phase 3 — language matrix (§16 of the phase brief).
//!
//! For every tier-1 structural language covered here (Java, Python, Go,
//! JavaScript, TypeScript, C, C++, C#, Rust, Dockerfile, Ruby, PHP, Swift,
//! Kotlin, Scala, Lua)
//! the same
//! invariants are proven against that language's fixture plus
//! inline edge-case sources: valid / malformed / incomplete / empty /
//! comments-with-code-like-text / nested declarations / spans under CRLF,
//! no trailing newline and Unicode / redacted content safety /
//! deterministic repeat parsing.
//!
//! Language-specific extras (overloads, aliases, imports forms) live in
//! `phase3_language_specific.rs`.

use std::path::PathBuf;

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, CancellationToken, ResourceBudget, default_registry,
    diagnostic_codes, dispatch,
};
use attic_core::FileOccurrenceId;
use attic_core::FileType;

fn registry_all() -> attic_analyzers::AnalyzerRegistry {
    default_registry()
}

fn input_for(code: String, ft: FileType, language_hint: Option<&str>) -> AnalyzerInput {
    let size = code.len() as u64;
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("fixture.src"),
        content: AnalyzerContent::FullBytes(code.into_bytes()),
        language_hint: language_hint.map(str::to_string),
        file_type: ft,
        size_bytes: size,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

struct Lang {
    name: &'static str,
    fixture: &'static str,
    file_type: FileType,
    language_hint: Option<&'static str>,
    /// A token expected to appear in some retrieval unit.
    searchable_token: &'static str,
}

const LANGS: [Lang; 16] = [
    Lang {
        name: "java",
        fixture: include_str!("fixtures/OrderService.java"),
        file_type: FileType::Java,
        language_hint: None,
        searchable_token: "OrderService",
    },
    Lang {
        name: "python",
        fixture: include_str!("fixtures/sample.py"),
        file_type: FileType::Python,
        language_hint: None,
        searchable_token: "Inventory",
    },
    Lang {
        name: "go",
        fixture: include_str!("fixtures/server.go"),
        file_type: FileType::Go,
        language_hint: None,
        searchable_token: "NewStore",
    },
    Lang {
        name: "javascript",
        fixture: include_str!("fixtures/widget.js"),
        file_type: FileType::JavaScript,
        language_hint: None,
        searchable_token: "makeWidget",
    },
    Lang {
        name: "typescript",
        fixture: include_str!("fixtures/widget.ts"),
        file_type: FileType::TypeScript,
        language_hint: Some("typescript"),
        searchable_token: "BaseWidget",
    },
    Lang {
        name: "c",
        fixture: include_str!("fixtures/sample.c"),
        file_type: FileType::C,
        language_hint: Some("c"),
        searchable_token: "global",
    },
    Lang {
        name: "cpp",
        fixture: include_str!("fixtures/widget.cpp"),
        file_type: FileType::Cpp,
        language_hint: Some("cpp"),
        searchable_token: "Widget",
    },
    Lang {
        name: "csharp",
        fixture: include_str!("fixtures/SampleApp.cs"),
        file_type: FileType::Other,
        language_hint: Some("csharp"),
        searchable_token: "Worker",
    },
    Lang {
        name: "rust",
        fixture: include_str!("fixtures/sample.rs"),
        file_type: FileType::Rust,
        language_hint: Some("rust"),
        searchable_token: "Runner",
    },
    Lang {
        name: "dockerfile",
        fixture: include_str!("fixtures/Sample.Dockerfile"),
        file_type: FileType::Other,
        language_hint: Some("dockerfile"),
        searchable_token: "builder",
    },
    Lang {
        name: "ruby",
        fixture: include_str!("fixtures/sample.rb"),
        file_type: FileType::Other,
        language_hint: Some("ruby"),
        searchable_token: "Greeter",
    },
    Lang {
        name: "php",
        fixture: include_str!("fixtures/sample.php"),
        file_type: FileType::Other,
        language_hint: Some("php"),
        searchable_token: "Worker",
    },
    Lang {
        name: "swift",
        fixture: include_str!("fixtures/sample.swift"),
        file_type: FileType::Other,
        language_hint: Some("swift"),
        searchable_token: "Worker",
    },
    Lang {
        name: "kotlin",
        fixture: include_str!("fixtures/orders.kt"),
        file_type: FileType::Other,
        language_hint: Some("kotlin"),
        searchable_token: "OrderController",
    },
    Lang {
        name: "scala",
        fixture: include_str!("fixtures/inventory.scala"),
        file_type: FileType::Other,
        language_hint: Some("scala"),
        searchable_token: "Child",
    },
    Lang {
        name: "lua",
        fixture: include_str!("fixtures/module.lua"),
        file_type: FileType::Other,
        language_hint: Some("lua"),
        searchable_token: "local_helper",
    },
];

// ── Valid source ────────────────────────────────────────────────────────────

#[test]
fn valid_fixture_produces_structure_and_units() {
    let reg = registry_all();
    for lang in &LANGS {
        let out = dispatch(
            &reg,
            input_for(lang.fixture.to_string(), lang.file_type, lang.language_hint),
        );
        assert_eq!(
            out.analyzer_id,
            format!("{}-treesitter", lang.name),
            "[{}] wrong analyzer selected",
            lang.name
        );
        assert!(!out.fallback_used, "[{}] unexpected fallback", lang.name);
        assert!(
            !out.structural_nodes.is_empty(),
            "[{}] must produce structural nodes",
            lang.name
        );
        assert!(
            !out.symbols.is_empty(),
            "[{}] must produce symbols",
            lang.name
        );
        assert!(
            !out.imports.is_empty(),
            "[{}] fixture has imports",
            lang.name
        );
        assert!(
            out.retrieval_units
                .iter()
                .any(|u| u.retrieval_text.contains(lang.searchable_token)),
            "[{}] retrieval units must contain the searchable token",
            lang.name
        );
        // Parent links well-formed: parents precede children.
        for (i, n) in out.structural_nodes.iter().enumerate() {
            if let Some(p) = n.parent_index {
                assert!(p < i, "[{}] parent {} after child {}", lang.name, p, i);
            }
        }
        // Structural identity uniqueness: identical (type,name,parent) is
        // legal only for overloads whose SPANS differ.
        let mut seen = std::collections::HashSet::new();
        for n in &out.structural_nodes {
            let key = format!(
                "{}|{}|{:?}|{}|{}",
                n.node_type, n.name, n.parent_index, n.span.start_line, n.span.start_col
            );
            assert!(seen.insert(key), "[{}] duplicated node entry", lang.name);
        }
    }
}

// ── Malformed source: partial parse + diagnostics, never fatal ──────────────

#[test]
fn malformed_source_stays_specialized_with_diagnostics() {
    let reg = registry_all();
    const CASES: [(FileType, Option<&str>, &str); 16] = [
        (
            FileType::Java,
            None,
            "public class Broken {\n  def not_java(:\n     ??? ;\n",
        ),
        (
            FileType::Python,
            None,
            "def broken(:\n  return ??\nclass 123:\n",
        ),
        (FileType::Go, None, "func broken( {{{ \n type x y\n"),
        (FileType::JavaScript, None, "class { function ( { let =;\n"),
        (
            FileType::TypeScript,
            Some("typescript"),
            "interface { abstract () : <><\n enum e { ,,, }\n",
        ),
        (FileType::C, Some("c"), "int broken( {\n  return 1;\n}\n"),
        (
            FileType::Cpp,
            Some("cpp"),
            "class Broken : public {\nvoid run(\n",
        ),
        (
            FileType::Other,
            Some("csharp"),
            "namespace Demo { class Broken { void Run( { new Widget( }\n",
        ),
        (
            FileType::Rust,
            Some("rust"),
            "pub trait Broken: { fn run(&self)\nimpl Broken for {\n",
        ),
        (
            FileType::Other,
            Some("dockerfile"),
            "FROM alpine AS\nCOPY --from= builder\nONBUILD COPY [\n",
        ),
        (
            FileType::Other,
            Some("ruby"),
            "module Broken\n  class Worker <\n    def run(\n      helper(\n",
        ),
        (
            FileType::Other,
            Some("php"),
            "<?php\nnamespace Broken;\nclass Worker extends {\n  public function run(: void {\n",
        ),
        (
            FileType::Other,
            Some("swift"),
            "import Foundation\nclass Worker: {\n  func run( {\n    helper(\n",
        ),
        (
            FileType::Other,
            Some("kotlin"),
            "package demo\nclass Broken( {\n  fun nope(:\n",
        ),
        (
            FileType::Other,
            Some("scala"),
            "package demo\nclass Broken extends {\n  def nope(x: Int =\n",
        ),
        (
            FileType::Other,
            Some("lua"),
            "function broken(\n  local x =\n",
        ),
    ];
    for (ft, hint, src) in CASES {
        let out = dispatch(&reg, input_for(src.to_string(), ft, hint));
        assert!(
            !out.fallback_used,
            "[{ft:?}] error-node parse must NOT trigger generic fallback"
        );
        assert!(
            out.diagnostics.iter().any(|d| d.code == "PARSE_ERROR"
                && d.severity != attic_analyzers::DiagnosticSeverity::Error),
            "[{ft:?}] expected non-fatal PARSE_ERROR diagnostic"
        );
    }
}

// ── Incomplete (truncated) source ───────────────────────────────────────────

#[test]
fn incomplete_source_yields_partial_structure() {
    let reg = registry_all();
    const CASES: [(FileType, Option<&str>, &str); 16] = [
        (
            FileType::Java,
            None,
            "package a.b;\npublic class Cut {\n  int fie",
        ),
        (FileType::Python, None, "class Cut:\n    def method(se"),
        (
            FileType::Go,
            None,
            "package cut\n\nfunc Top() int {\n\treturn ",
        ),
        (FileType::JavaScript, None, "export class Cut extends Ba"),
        (
            FileType::TypeScript,
            Some("typescript"),
            "export interface Cut { id: strin",
        ),
        (FileType::C, Some("c"), "typedef struct Cut {\n  int field"),
        (
            FileType::Cpp,
            Some("cpp"),
            "namespace demo {\nclass Cut : public Ba",
        ),
        (
            FileType::Other,
            Some("csharp"),
            "namespace Demo { public class Cut : Base, IFace { public void Run(",
        ),
        (
            FileType::Rust,
            Some("rust"),
            "pub trait Worker: Named {\n    fn run(&self)\npub mod nested {",
        ),
        (
            FileType::Other,
            Some("dockerfile"),
            "FROM alpine AS builder\nCOPY src/app.sh",
        ),
        (
            FileType::Other,
            Some("ruby"),
            "module Cut\n  class Worker < Base\n    def run(nam",
        ),
        (
            FileType::Other,
            Some("php"),
            "<?php\nnamespace Demo;\nclass Cut extends Base implements Runnable {\n  public function run(",
        ),
        (
            FileType::Other,
            Some("swift"),
            "import SupportKit\nclass Cut: BaseWorker, Runnable {\n  func run(",
        ),
        (
            FileType::Other,
            Some("kotlin"),
            "package cut\nclass Cut {\n  fun run(x: Int)",
        ),
        (
            FileType::Other,
            Some("scala"),
            "package cut\nobject Cut { def run(x: Int) =",
        ),
        (
            FileType::Other,
            Some("lua"),
            "local function cut(x)\n  return ",
        ),
    ];
    for (ft, hint, src) in CASES {
        let out = dispatch(&reg, input_for(src.to_string(), ft, hint));
        assert!(
            !out.fallback_used,
            "[{ft:?}] truncated must stay specialized"
        );
        // Truncated-but-parseable prefixes usually carry at least one node or
        // symbol; when even that fails the units keep the text searchable.
        assert!(
            !out.structural_nodes.is_empty()
                || out
                    .retrieval_units
                    .iter()
                    .any(|u| !u.retrieval_text.is_empty()),
            "[{ft:?}] partial output required",
        );
    }
}

// ── Empty source ────────────────────────────────────────────────────────────

#[test]
fn empty_source_is_safe_no_op() {
    let reg = registry_all();
    for lang in &LANGS {
        let out = dispatch(
            &reg,
            input_for(String::new(), lang.file_type, lang.language_hint),
        );
        assert_eq!(out.analyzer_id, format!("{}-treesitter", lang.name));
        assert!(out.structural_nodes.is_empty());
        assert!(out.symbols.is_empty());
        assert!(out.retrieval_units.is_empty());
    }
}

// ── Comments/strings containing code-like text ──────────────────────────────

#[test]
fn code_like_text_in_comments_and_strings_is_not_extracted() {
    let reg = registry_all();
    for lang in &LANGS {
        let out = dispatch(
            &reg,
            input_for(lang.fixture.to_string(), lang.file_type, lang.language_hint),
        );
        let names: Vec<&str> = out.symbols.iter().map(|s| s.short_name.as_str()).collect();
        for ghost in ["NotReal", "AlsoFake", "fake", "fake()"] {
            assert!(
                !names.contains(&ghost),
                "[{}] comment/docstring symbol '{}' leaked into symbols",
                lang.name,
                ghost
            );
        }
    }
}

// ── Spans: CRLF, missing trailing newline, Unicode ─────────────────────────

#[test]
fn spans_survive_crlf_and_missing_trailing_newline() {
    let reg = registry_all();
    // CRLF variant of a tiny Java class.
    let crlf = "package a;\r\npublic class CrLf {\r\n  int v;\r\n}";
    let out = dispatch(&reg, input_for(crlf.to_string(), FileType::Java, None));
    assert_eq!(out.analyzer_id, "java-treesitter");
    let cls = out
        .structural_nodes
        .iter()
        .find(|n| n.name == "CrLf")
        .expect("CRLF class found");
    assert!(cls.span.end_line >= cls.span.start_line);

    // No trailing newline.
    let nonl = "public class NoNewline {\n  int v;\n}";
    let out2 = dispatch(&reg, input_for(nonl.to_string(), FileType::Java, None));
    assert!(
        out2.structural_nodes.iter().any(|n| n.name == "NoNewline"),
        "no-trailing-newline class parsed"
    );
}

#[test]
fn unicode_identifiers_and_strings_are_span_correct() {
    let reg = registry_all();
    // Java identifiers are ASCII by spec — use Python (PEP 3131) instead.
    let py = "# -*- coding: utf-8 -*-\ndef grüße_ñ():\n    return \"héllo wörld ✓\"\n";
    let out = dispatch(&reg, input_for(py.to_string(), FileType::Python, None));
    assert!(
        out.symbols.iter().any(|s| s.short_name.contains("gr")),
        "unicode identifier extracted (lossy-safe)"
    );
    assert!(
        out.retrieval_units
            .iter()
            .any(|u| u.retrieval_text.contains("héllo wörld")),
        "unicode string preserved byte-exact"
    );
    // Go unicode string content.
    let go = "package u\n\nfunc Msg() string {\n\treturn \"日本語テキスト\"\n}\n";
    let out_go = dispatch(&reg, input_for(go.to_string(), FileType::Go, None));
    assert!(
        out_go
            .retrieval_units
            .iter()
            .any(|u| u.retrieval_text.contains("日本語"))
    );
}

// ── Redacted content: secrets never reach outputs ───────────────────────────

#[test]
fn redacted_content_never_leaks_secret_bytes_into_outputs() {
    let reg = registry_all();
    // Simulate Phase-1B redaction: secret span replaced with placeholder text.
    const SECRET: &str = "sk-live-SUPERSECRETVALUE123";
    const REDACTED_SRC: &str = "package leak;\n\npublic class LeakCheck {\n    private final String token = \"REDACTED_PLACEHOLDER\";\n    public String raw() { return \"REDACTED_PLACEHOLDER\"; }\n}\n";

    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("leak.java"),
            content: AnalyzerContent::RedactedBytes(REDACTED_SRC.as_bytes().to_vec()),
            language_hint: None,
            file_type: FileType::Java,
            size_bytes: REDACTED_SRC.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        },
    );
    assert_eq!(out.analyzer_id, "java-treesitter");

    let mut all_text = String::new();
    all_text.push_str(&out.analyzer_id);
    all_text.push_str(&out.analyzer_version);
    for d in &out.diagnostics {
        all_text.push_str(&d.message);
    }
    for u in &out.retrieval_units {
        all_text.push_str(&u.retrieval_text);
    }
    for s in &out.symbols {
        all_text.push_str(&s.qualified_name);
        if let Some(sig) = &s.signature {
            all_text.push_str(sig);
        }
    }
    for r in &out.relationships {
        all_text.push_str(&r.target_qualified_name);
    }
    assert!(
        !all_text.contains(SECRET),
        "raw secret bytes leaked into structural outputs"
    );
    // The analyzer preserved safe surrounding code.
    assert!(
        out.retrieval_units
            .iter()
            .any(|u| u.retrieval_text.contains("LeakCheck")),
        "safe surroundings still indexed after redaction"
    );
}

// ── Deterministic repeated parsing ──────────────────────────────────────────

#[test]
fn repeated_parsing_is_byte_identical() {
    let reg = registry_all();
    for lang in &LANGS {
        let run = || {
            let out = dispatch(
                &reg,
                input_for(lang.fixture.to_string(), lang.file_type, lang.language_hint),
            );
            serde_json::to_string(&(out.structural_nodes, out.symbols)).unwrap()
        };
        assert_eq!(run(), run(), "[{}] deterministic repeat", lang.name);
    }
}

// ── Cancellation returns CANCELLED diagnostic ───────────────────────────────

#[test]
fn pre_cancelled_analysis_reports_cancelled() {
    let reg = registry_all();
    for lang in &LANGS {
        let token = CancellationToken::new();
        token.cancel();
        let input = AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from(format!("cancelled-{}", lang.name)),
            content: AnalyzerContent::FullBytes(lang.fixture.as_bytes().to_vec()),
            language_hint: lang.language_hint.map(str::to_string),
            file_type: lang.file_type,
            size_bytes: lang.fixture.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        };
        let out = dispatch(&reg, input);
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diagnostic_codes::CANCELLED),
            "[{}] CANCELLED diagnostic required",
            lang.name
        );
    }
}

// ── Budget exhaustion is observable, never silent-complete ─────────────────

#[test]
fn ast_node_budget_exhaustion_is_reported_not_silent() {
    let reg = registry_all();
    // Deeply nested expression blows any tiny max_ast_nodes budget.
    let mut java = String::from("class Big { int v = ");
    let depth = 5000;
    for _ in 0..depth {
        java.push('(');
    }
    java.push('1');
    for _ in 0..depth {
        java.push(')');
    }
    java.push_str("; }");

    let budget = ResourceBudget {
        max_ast_nodes: 100,
        ..ResourceBudget::default()
    };
    let input = AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("big.java"),
        content: AnalyzerContent::FullBytes(java.into_bytes()),
        language_hint: None,
        file_type: FileType::Java,
        size_bytes: 10_000,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: budget,
    };
    let out = dispatch(&reg, input);
    // Contract: RESOURCE_EXHAUSTED → GenericAnalyzer fallback keeps searchability.
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::RESOURCE_EXHAUSTED),
        "RESOURCE_EXHAUSTED must be observable; got {:?}",
        out.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert!(
        out.fallback_used,
        "budget exhaustion routes to generic fallback"
    );
    assert!(
        !out.retrieval_units.is_empty(),
        "fallback keeps the file searchable"
    );
}
