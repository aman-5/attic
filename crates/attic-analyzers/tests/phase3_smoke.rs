//! Phase 3 smoke: engine produces canonical output for each language.

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResourceBudget,
};
use attic_core::{FileOccurrenceId, FileType, SymbolKind};

fn input(code: &'static str, ft: FileType) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("t.src"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: None,
        file_type: ft,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

#[test]
fn smoke_all_languages() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());
    reg.register_specialized(attic_analyzers::structural::python::analyzer());
    reg.register_specialized(attic_analyzers::structural::go::analyzer());
    reg.register_specialized(attic_analyzers::structural::javascript::analyzer());
    reg.register_specialized(attic_analyzers::structural::typescript::analyzer());

    const JAVA_SRC: &str = r#"package com.example.app;

import java.util.List;
import static java.lang.Math.max;

public class FooService extends Base implements AutoCloseable {
    private static final int LIMIT = 10;
    private List<String> items;

    public FooService(List<String> items) { this.items = items; }

    protected int compute(int a, int b) {
        return max(a, b) + LIMIT + items.size();
    }

    public void helper() { compute(1,2); }
}
"#;
    let out = attic_analyzers::dispatch(&reg, input(JAVA_SRC, FileType::Java));
    println!("== JAVA ==");
    println!(
        "analyzer={} fallback={}",
        out.analyzer_id, out.fallback_used
    );
    for d in &out.diagnostics {
        println!("diag {} {}", d.severity as u8, d.code);
    }
    for n in &out.structural_nodes {
        println!("node {} {} parent{:?}", n.node_type, n.name, n.parent_index);
    }
    for s in &out.symbols {
        println!("sym {:?} {} pub={}", s.kind, s.qualified_name, s.is_public);
    }
    for i in &out.imports {
        println!("imp [{}] {}", i.import_kind, i.raw_specifier);
    }
    for r in &out.relationships {
        println!(
            "rel {} -> {} ({:?},{})",
            r.relationship_type, r.target_qualified_name, r.resolution, r.confidence
        );
    }
    for u in &out.retrieval_units {
        println!(
            "unit ord={} node={:?} lines={}-{} bytes={}",
            u.ordinal,
            u.structural_node_index,
            u.span.start_line,
            u.span.end_line,
            u.retrieval_text.len()
        );
    }

    assert_eq!(out.analyzer_id, "java-treesitter");
    assert!(!out.fallback_used);
    assert!(out.structural_nodes.len() >= 3, "class + ctor + methods");
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name == "com.example.app.FooService")
    );
    assert!(
        out.imports
            .iter()
            .any(|i| i.raw_specifier == "java.util.List")
    );
    // intra-file call resolved
    assert!(
        out.relationships
            .iter()
            .any(|r| r.relationship_type == "CALL"
                && r.target_qualified_name == "compute"
                && r.confidence >= 0.9)
    );

    const PY_SRC: &str = r##"import os
from . import sibling

MAX_K = 10

def top(a):
    return helper(a)

def helper(x):
    return x * 2

class Base:
    def __init__(self):
        self.v = top(1)
"##;
    let out_py = attic_analyzers::dispatch(&reg, input(PY_SRC, FileType::Python));
    println!("== PYTHON ==");
    println!(
        "analyzer={} fallback={}",
        out_py.analyzer_id, out_py.fallback_used
    );
    for s in &out_py.symbols {
        println!("sym {:?} {}", s.kind, s.qualified_name);
    }
    for i in &out_py.imports {
        println!("imp [{}] {}", i.import_kind, i.raw_specifier);
    }
    for r in &out_py.relationships {
        println!(
            "rel {} -> {} {:?}",
            r.relationship_type, r.target_qualified_name, r.resolution
        );
    }
    assert_eq!(out_py.analyzer_id, "python-treesitter");
    assert!(
        out_py
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Base" && s.kind == SymbolKind::Class)
    );
    assert!(
        out_py
            .imports
            .iter()
            .any(|i| i.raw_specifier == ".:sibling" || i.raw_specifier == "os")
    );
}

#[test]
fn smoke_go_js_ts() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::go::analyzer());
    reg.register_specialized(attic_analyzers::structural::javascript::analyzer());
    reg.register_specialized(attic_analyzers::structural::typescript::analyzer());

    const GO_SRC: &str = "package server\n\nimport (\n\t\"fmt\"\n\t\"github.com/example/repo/internal/util\"\n)\n\nconst MaxSize = 100\n\ntype Handler struct {\n\tName string\n}\n\ntype Router interface {\n\tRoute(path string) error\n}\n\nfunc NewHandler(name string) *Handler {\n\treturn &Handler{Name: name}\n}\n\nfunc (h *Handler) Serve() {\n\tNewHandler(\"x\")\n\tfmt.Sprint(h.Name)\n}\n";
    let out = attic_analyzers::dispatch(&reg, input(GO_SRC, FileType::Go));
    println!("== GO ==");
    for s in &out.symbols {
        println!("sym {:?} {}", s.kind, s.qualified_name);
    }
    for i in &out.imports {
        println!("imp [{}] {}", i.import_kind, i.raw_specifier);
    }
    for r in &out.relationships {
        println!(
            "rel {} -> {} {:?}",
            r.relationship_type, r.target_qualified_name, r.resolution
        );
    }
    assert_eq!(out.analyzer_id, "go-treesitter");
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name == "server.Handler" && s.kind == SymbolKind::Class)
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.qualified_name == "server.Router.Route")
    );
    assert!(
        out.symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Method && s.qualified_name.ends_with(".Handler.Serve"))
    );
    assert!(
        out.imports
            .iter()
            .any(|i| i.raw_specifier == "github.com/example/repo/internal/util")
    );

    const JS_SRC: &str = "import { helper } from './util.js';\nconst legacy = require('./legacy.cjs');\nexport * from './re.js';\nexport const VERSION = '1';\nexport class Widget extends Base {\n  #priv = 1;\n  render(props) { return helper(props); }\n}\nexport function make() { return new Widget(); }\nconst arrow = (a) => a + 1;\n";
    let out_js = attic_analyzers::dispatch(&reg, input(JS_SRC, FileType::JavaScript));
    println!("== JS ==");
    for s in &out_js.symbols {
        println!("sym {:?} {} pub={}", s.kind, s.qualified_name, s.is_public);
    }
    for i in &out_js.imports {
        println!("imp [{}] {}", i.import_kind, i.raw_specifier);
    }
    for r in &out_js.relationships {
        println!(
            "rel {} -> {} {:?}",
            r.relationship_type, r.target_qualified_name, r.resolution
        );
    }
    assert_eq!(out_js.analyzer_id, "javascript-treesitter");
    assert!(
        out_js
            .imports
            .iter()
            .any(|i| i.import_kind == "REQUIRE" && i.raw_specifier == "./legacy.cjs")
    );
    assert!(
        out_js
            .imports
            .iter()
            .any(|i| i.import_kind == "EXPORT_FROM" && i.raw_specifier == "./re.js")
    );
    assert!(
        out_js
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Widget.render" && s.is_public)
    );
    assert!(
        out_js
            .relationships
            .iter()
            .any(|r| r.relationship_type == "EXTENDS" && r.target_qualified_name == "Base")
    );
    // Calls to imported bindings are honest REFERENCES to their module.
    assert!(
        out_js
            .relationships
            .iter()
            .any(|r| r.relationship_type == "REFERENCES"
                && r.target_qualified_name == "./util.js"
                && r.resolution == attic_analyzers::ResolutionLevel::Syntactic)
    );
    assert!(
        out_js
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Function && s.short_name == "arrow")
    );

    const TS_SRC: &str = "import type { Config } from './config';\nexport interface Options { id: string; run?(): void }\nexport type Alias = Options | null;\nexport enum Color { Red = 1 }\nexport namespace Util { export function clamp(v: number): number { return v; } }\nexport abstract class BaseWidget extends Vue implements Component {\n  abstract render(): string;\n  get value(): number { return 1; }\n}\n";
    let out_ts = attic_analyzers::dispatch(&reg, input(TS_SRC, FileType::TypeScript));
    println!("== TS ==");
    for n in &out_ts.structural_nodes {
        println!("node {} {}", n.node_type, n.name);
    }
    for s in &out_ts.symbols {
        println!(
            "sym {:?} {} def={}",
            s.kind, s.qualified_name, s.is_definition
        );
    }
    for i in &out_ts.imports {
        println!("imp [{}] {}", i.import_kind, i.raw_specifier);
    }
    for r in &out_ts.relationships {
        println!(
            "rel {} -> {} {:?}",
            r.relationship_type, r.target_qualified_name, r.resolution
        );
    }
    assert_eq!(out_ts.analyzer_id, "typescript-treesitter");
    assert!(
        out_ts
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Interface && s.qualified_name == "Options")
    );
    assert!(
        out_ts
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::TypeAlias && s.short_name == "Alias")
    );
    assert!(
        out_ts
            .symbols
            .iter()
            .any(|s| s.kind == SymbolKind::Module && s.short_name == "Util")
    );
    assert!(out_ts.symbols.iter().any(|s| s.kind == SymbolKind::Method
        && s.qualified_name == "Options.run"
        && !s.is_definition));
    assert!(
        out_ts
            .imports
            .iter()
            .any(|i| i.import_kind == "IMPORT_TYPE" && i.raw_specifier == "./config")
    );
    let impl_rel = out_ts
        .relationships
        .iter()
        .find(|r| r.relationship_type == "IMPLEMENTS")
        .expect("implements edge");
    assert_eq!(impl_rel.target_qualified_name, "Component");
}
