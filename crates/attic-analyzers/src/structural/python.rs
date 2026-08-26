//! Python language specification (grammar: `tree-sitter-python` 0.25.x).
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `module`, `import_statement` (`dotted_name`, `aliased_import`),
//! `import_from_statement` (`relative_import` → `import_prefix` + optional
//! `dotted_name`; imported names as `dotted_name` / `aliased_import` /
//! `wildcard_import`), `class_definition` (name, `argument_list`
//! superclasses, body), `function_definition` (+ anonymous `async`),
//! `decorated_definition` → `decorator` + definition,
//! `assignment`, `annotated_assignment`(observed as `assignment` with
//! intervening `type` node), `call`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static PYTHON_SPEC: PythonSpec = PythonSpec;

pub struct PythonSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&PYTHON_SPEC)
}

impl TreeSitterLanguageSpec for PythonSpec {
    fn analyzer_id(&self) -> &'static str {
        "python-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Python: structure, symbols \
         (classes, functions/methods incl. async/decorated, module \
         constants), imports with relative-import semantics, and intra-file \
         references."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Python]
    }

    fn capabilities(&self) -> AnalyzerCapabilities {
        AnalyzerCapabilities {
            entries: vec![
                (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                (CapabilityKind::SymbolExtraction, CapabilityLevel::Full),
                // Relative imports are resolvable against repo layout; the
                // syntax layer records the raw facts only.
                (CapabilityKind::ImportExtraction, CapabilityLevel::Full),
                (CapabilityKind::ReferenceExtraction, CapabilityLevel::Basic),
                (
                    CapabilityKind::RelationshipResolution,
                    CapabilityLevel::Basic,
                ),
            ],
        }
    }

    fn grammar(&self) -> tree_sitter_language::LanguageFn {
        tree_sitter_python::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "python"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
        };
        let scope: Vec<String> = Vec::new();
        extract_block_children(&mut st, root, &scope, None, true, out);
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
}

fn extract_block_children(
    st: &mut St<'_>,
    container: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    for child in named_children(container) {
        if !out.tick() {
            return;
        }
        match child.kind() {
            "import_statement" => extract_import(child, st.src, out),
            "import_from_statement" => extract_import_from(child, st.src, out),
            "class_definition" | "decorated_definition" => {
                extract_class(st, child, scope, parent_idx, top_level, out)
            }
            "function_definition" | "decorated_function" => {
                extract_function(st, child, scope, parent_idx, top_level, false, out);
            }
            "expression_statement" => {
                if top_level {
                    extract_module_assignment(st, child, out);
                    collect_calls_in(st, child, None, out);
                } else {
                    collect_calls_in(st, child, None, out);
                }
            }
            "comment" => {}
            _ => collect_calls_in(st, child, None, out),
        }
    }
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

fn unwrap_decorated(node: Node<'_>) -> Node<'_> {
    if node.kind() == "decorated_definition" {
        named_children(node)
            .into_iter()
            .find(|n| n.kind() == "class_definition" || n.kind() == "function_definition")
            .unwrap_or(node)
    } else {
        node
    }
}

fn extract_class(
    st: &mut St<'_>,
    wrapper: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let def = unwrap_decorated(wrapper);
    if def.kind() != "class_definition" {
        return;
    }
    if !out.tick() {
        return;
    }
    let Some(name_node) = def.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);

    let mut qparts: Vec<String> = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");

    // The pushed node spans decorators too (wrapper), when present.
    let span_node = if wrapper.kind() == "decorated_definition" {
        wrapper
    } else {
        def
    };
    let identity = format!("python|{qualified}|CLASS");
    let Some(idx) = out.push_node("CLASS", &name, span_node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Class,
        span: span_of(span_node),
        is_public: !name.starts_with('_'),
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    // Base classes: positional args of the superclasses argument_list.
    if let Some(args) = def.child_by_field_name("superclasses") {
        let mut c = args.walk();
        for ch in args.children(&mut c) {
            if !ch.is_named() || ch.kind() == "keyword_argument" {
                continue;
            }
            out.push_rel(
                "EXTENDS",
                text(ch, st.src),
                ch,
                ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    if top_level {
        out.mark_top_level(idx);
    }

    // Class body: methods at one nesting level, nested classes recurse.
    let mut next_scope = qparts;
    next_scope.pop();
    next_scope.push(name);
    if let Some(body) = def.child_by_field_name("body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "function_definition" | "decorated_definition" => {
                    extract_method_or_nested(st, member, &qualified, Some(idx), out);
                }
                "class_definition" => {
                    extract_class(st, member, &next_scope, Some(idx), false, out);
                }
                _ => {}
            }
        }
    }
}

fn extract_method_or_nested(
    st: &mut St<'_>,
    wrapper: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    out: &mut Extraction<'_>,
) {
    let def = unwrap_decorated(wrapper);
    if !out.tick() {
        return;
    }
    if def.kind() == "class_definition" {
        return; // handled by caller
    }
    let Some(name_node) = def.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = def.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let is_async = has_async(def);
    let signature = format!("{name}{params_text}");

    let span_node = if wrapper.kind() == "decorated_definition" {
        wrapper
    } else {
        def
    };
    let qualified = format!("{owner_qualified}.{name}");
    let identity = format!(
        "python|{qualified}|METHOD|{signature}{}",
        if is_async { "|async" } else { "" }
    );
    let Some(idx) = out.push_node("METHOD", &name, span_node, identity, owner_node) else {
        return;
    };

    let method_sym = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(span_node),
        is_public: !name.starts_with('_'),
        disambiguator: None,
        signature: Some(signature),
        visibility: Some(if name.starts_with("__") {
            "name-mangled".to_string()
        } else if name.starts_with('_') {
            "private".to_string()
        } else {
            "public".to_string()
        }),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    // Nested function definitions inside the method body.
    if let Some(body) = def.child_by_field_name("body") {
        for inner in named_children(body) {
            if inner.kind() == "function_definition" {
                extract_function(
                    st,
                    inner,
                    &[owner_qualified.to_string(), name.clone()],
                    None,
                    false,
                    is_async,
                    out,
                );
            } else if !out.tick() {
                return;
            }
        }
        collect_calls_in(st, body, Some(method_sym), out);
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_function(
    st: &mut St<'_>,
    def: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    _in_async_parent: bool,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let Some(name_node) = def.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = def.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let is_async = has_async(def);
    let signature = format!("{name}{params_text}");

    let mut qparts: Vec<String> = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");
    let identity = format!(
        "python|{qualified}|FUNCTION|{signature}{}",
        if is_async { "|async" } else { "" }
    );
    let Some(idx) = out.push_node("FUNCTION", &name, def, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Function,
        span: span_of(def),
        is_public: !name.starts_with('_'),
        disambiguator: None,
        signature: Some(signature),
        visibility: Some(if name.starts_with("__") {
            "name-mangled".to_string()
        } else if name.starts_with('_') {
            "private".to_string()
        } else {
            "public".to_string()
        }),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name);

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = def.child_by_field_name("body") {
        collect_calls_in(st, body, Some(sym_idx), out);
        // Nested defs (one level of explicit recursion).
        for inner in named_children(body) {
            if inner.kind() == "function_definition" {
                let nested_scope: Vec<String> = qparts[..qparts.len().saturating_sub(1)].to_vec();
                extract_function(st, inner, &nested_scope, Some(idx), false, is_async, out);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Module-level assignments → constants
// ---------------------------------------------------------------------------

fn extract_module_assignment(st: &mut St<'_>, stmt: Node<'_>, out: &mut Extraction<'_>) {
    let Some(assign) = named_children(stmt)
        .into_iter()
        .find(|n| n.kind() == "assignment")
    else {
        return;
    };
    let Some(lhs) = assign.child_by_field_name("left") else {
        return;
    };
    if lhs.kind() != "identifier" {
        return;
    }
    let name = text(lhs, st.src);
    if !is_upper_const(&name) {
        return;
    }
    out.push_symbol(CanonSymbol {
        qualified_name: name.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Constant,
        span: span_of(stmt),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: Some("public".to_string()),
        is_definition: true,
        node_index: None,
    });
    st.locals.push(name);
}

fn is_upper_const(s: &str) -> bool {
    s.chars().any(char::is_alphabetic)
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn extract_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    for ch in named_children(node) {
        match ch.kind() {
            "dotted_name" => out.push_import(text(ch, src), "IMPORT", node),
            "aliased_import" => {
                if let Some(dn) = ch.child_by_field_name("name") {
                    out.push_import(text(dn, src), "IMPORT", node);
                }
            }
            _ => {}
        }
    }
}

fn extract_import_from(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    // Grammar 0.25: fields `module_name` (dotted_name | relative_import) and
    // one-or-more `name` entries (dotted_name | aliased_import |
    // wildcard_import).
    let module_node = node.child_by_field_name("module_name");
    let module_text = module_node
        .map(|m| relative_import_text(m, src))
        .unwrap_or_default();
    for ch in named_children(node) {
        if let Some(m) = module_node
            && ch == m
        {
            continue;
        }
        match ch.kind() {
            "dotted_name" => {
                out.push_import(format!("{module_text}:{}", text(ch, src)), "FROM", node);
            }
            "aliased_import" => {
                if let Some(dn) = ch.child_by_field_name("name") {
                    out.push_import(format!("{module_text}:{}", text(dn, src)), "FROM", node);
                }
            }
            "wildcard_import" => {
                out.push_import(format!("{module_text}:*"), "FROM", node);
            }
            _ => {}
        }
    }
}

fn relative_import_text(node: Node<'_>, src: &SourceText<'_>) -> String {
    if node.kind() == "dotted_name" {
        return text(node, src);
    }
    // relative_import
    let dots = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "import_prefix")
        .map(|p| text(p, src))
        .unwrap_or_else(|| ".".to_string());
    let tail = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "dotted_name")
        .map(|d| text(d, src));
    match tail {
        Some(t) => format!("{dots}{t}"),
        None => dots,
    }
}

// ---------------------------------------------------------------------------
// Intra-file references
// ---------------------------------------------------------------------------

fn collect_calls_in(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_sym: Option<usize>,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    if node.kind() == "call"
        && let Some(func) = node.child_by_field_name("function")
        && func.kind() == "identifier"
    {
        let callee = text(func, st.src);
        if st.locals.contains(&callee) {
            out.push_rel(
                "CALL",
                callee,
                node,
                ResolutionLevel::SymbolResolved,
                0.85,
                owner_sym,
            );
            return; // don't re-walk the same call's children for calls
        }
    }
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        collect_calls_in(st, ch, owner_sym, out);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn has_async(fn_def: Node<'_>) -> bool {
    let mut c = fn_def.walk();
    fn_def
        .children(&mut c)
        .any(|ch| !ch.is_named() && ch.kind() == "async")
}
