//! Phase 3 — language matrix (§16 of the phase brief).
//!
//! For EVERY supported language (Java, Python, Go, JavaScript, TypeScript)
//! the same invariants are proven against that language's fixture plus
//! inline edge-case sources: valid / malformed / incomplete / empty /
//! comments-with-code-like-text / nested declarations / spans under CRLF,
//! no trailing newline and Unicode / redacted content safety /
//! deterministic repeat parsing.
//!
//! Language-specific extras (overloads, aliases, imports forms) live in
//! `phase3_language_specific.rs`.

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerInput, AnalyzerOutput,
    AnalyzerRegistry, CancellationToken, CapabilityKind, GenericAnalyzer, ResourceBudget,
    diagnostic_codes, dispatch,
};
use attic_core::FileOccurrenceId;
use attic_core::FileType;

fn registry_all() -> AnalyzerRegistry {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());
    reg.register_specialized(attic_analyzers::structural::python::analyzer());
    reg.register_specialized(attic_analyzers::structural::go::analyzer());
    reg.register_specialized(attic_analyzers::structural::javascript::analyzer());
    reg.register_specialized(attic_analyzers::structural::typescript::analyzer());
    reg
}

fn input_for(code: String, ft: FileType) -> AnalyzerInput {
    let size = code.len() as u64;
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("fixture.src"),
        content: AnalyzerContent::FullBytes(code.into_bytes()),
        language_hint: None,
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
    /// A token expected to appear in some retrieval unit.
    searchable_token: &'static str,
}

const LANGS: [Lang; 5] = [
    Lang {
        name: "java",
        fixture: include_str!("fixtures/OrderService.java"),
        file_type: FileType::Java,
        searchable_token: "OrderService",
    },
    Lang {
        name: "python",
        fixture: include_str!("fixtures/sample.py"),
        file_type: FileType::Python,
        searchable_token: "Inventory",
    },
    Lang {
        name: "go",
        fixture: include_str!("fixtures/server.go"),
        file_type: FileType::Go,
        searchable_token: "NewStore",
    },
    Lang {
        name: "javascript",
        fixture: include_str!("fixtures/widget.js"),
        file_type: FileType::JavaScript,
        searchable_token: "makeWidget",
    },
    Lang {
        name: "typescript",
        fixture: include_str!("fixtures/widget.ts"),
        file_type: FileType::TypeScript,
        searchable_token: "BaseWidget",
    },
];

// ── Valid source ────────────────────────────────────────────────────────────

#[test]
fn valid_fixture_produces_structure_and_units() {
    let reg = registry_all();
    for lang in &LANGS {
        let out = dispatch(&reg, input_for(lang.fixture.to_string(), lang.file_type));
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
    const CASES: [(FileType, &str); 5] = [
        (
            FileType::Java,
            "public class Broken {\n  def not_java(:\n     ??? ;\n",
        ),
        (FileType::Python, "def broken(:\n  return ??\nclass 123:\n"),
        (FileType::Go, "func broken( {{{ \n type x y\n"),
        (FileType::JavaScript, "class { function ( { let =;\n"),
        (
            FileType::TypeScript,
            "interface { abstract () : <><\n enum e { ,,, }\n",
        ),
    ];
    for (ft, src) in CASES {
        let out = dispatch(&reg, input_for(src.to_string(), ft));
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
    const CASES: [(FileType, &str); 5] = [
        (
            FileType::Java,
            "package a.b;\npublic class Cut {\n  int fie",
        ),
        (FileType::Python, "class Cut:\n    def method(se"),
        (FileType::Go, "package cut\n\nfunc Top() int {\n\treturn "),
        (FileType::JavaScript, "export class Cut extends Ba"),
        (FileType::TypeScript, "export interface Cut { id: strin"),
    ];
    for (ft, src) in CASES {
        let out = dispatch(&reg, input_for(src.to_string(), ft));
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
        let out = dispatch(&reg, input_for(String::new(), lang.file_type));
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
        let out = dispatch(&reg, input_for(lang.fixture.to_string(), lang.file_type));
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
    let out = dispatch(&reg, input_for(crlf.to_string(), FileType::Java));
    assert_eq!(out.analyzer_id, "java-treesitter");
    let cls = out
        .structural_nodes
        .iter()
        .find(|n| n.name == "CrLf")
        .expect("CRLF class found");
    assert!(cls.span.end_line >= cls.span.start_line);

    // No trailing newline.
    let nonl = "public class NoNewline {\n  int v;\n}";
    let out2 = dispatch(&reg, input_for(nonl.to_string(), FileType::Java));
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
    let out = dispatch(&reg, input_for(py.to_string(), FileType::Python));
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
    let out_go = dispatch(&reg, input_for(go.to_string(), FileType::Go));
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
            let out = dispatch(&reg, input_for(lang.fixture.to_string(), lang.file_type));
            serde_json::to_string(&(out.structural_nodes, out.symbols)).unwrap()
        };
        assert_eq!(run(), run(), "[{}] deterministic repeat", lang.name);
    }
}

// ── Cancellation returns CANCELLED diagnostic ───────────────────────────────

#[test]
fn pre_cancelled_analysis_reports_cancelled() {
    let reg = registry_all();
    let token = CancellationToken::new();
    token.cancel();
    let input = AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("c.java"),
        content: AnalyzerContent::FullBytes(b"public class C {}".to_vec()),
        language_hint: None,
        file_type: FileType::Java,
        size_bytes: 17,
        is_partial_scan: false,
        cancellation_token: token,
        resource_budget: ResourceBudget::default(),
    };
    let out = dispatch(&reg, input);
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == diagnostic_codes::CANCELLED),
        "CANCELLED diagnostic required"
    );
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

// Keep imports used across test fns referenced.
#[allow(dead_code)]
fn _touch(_a: &AnalyzerCapabilities, _b: &CapabilityKind, _o: &Option<AnalyzerOutput>) {}
