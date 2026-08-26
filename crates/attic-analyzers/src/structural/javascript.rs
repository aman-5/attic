//! JavaScript language specification (grammar: `tree-sitter-javascript`
//! 0.25.x). TypeScript reuses these shared ECMAScript walkers via
//! `pub(crate)` items and adds TS-only declarations on top.
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `program`, `import_statement` (`import_clause` → identifier /
//! `named_imports` → `import_specifier`; source `string` →
//! `string_fragment`), `export_statement`, `lexical_declaration`/
//! `variable_declaration` (`variable_declarator`), `function_declaration`,
//! `class_declaration` (`class_heritage`, `class_body` →
//! `method_definition` / `field_definition`), `call_expression`
//! (`require(...)` CommonJS and dynamic `import(...)`)， arrow functions
//! inside declarators.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, LanguageSpec, SourceText, make_analyzer, span_of,
};

pub(crate) static JAVASCRIPT_SPEC: JavaScriptSpec = JavaScriptSpec;

pub struct JavaScriptSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&JAVASCRIPT_SPEC)
}

impl LanguageSpec for JavaScriptSpec {
    fn analyzer_id(&self) -> &'static str {
        "javascript-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for JavaScript: structure, symbols \
         (classes, functions incl. arrows/generators, methods, fields, \
         module constants), ESM/CommonJS/dynamic imports, and intra-file \
         references."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::JavaScript]
    }

    fn capabilities(&self) -> AnalyzerCapabilities {
        AnalyzerCapabilities {
            entries: vec![
                (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                (CapabilityKind::SymbolExtraction, CapabilityLevel::Full),
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
        tree_sitter_javascript::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "javascript"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = JsState {
            lang_tag: "javascript",
            src,
            locals: Vec::new(),
            imported: Vec::new(),
        };
        walk_container(&mut st, root, &[], None, true, out);
    }
}

// ---------------------------------------------------------------------------
// Shared ECMAScript state/walkers (reused by the TypeScript spec)
// ---------------------------------------------------------------------------

pub(crate) struct JsState<'s> {
    /// Baked into every identity basis (`javascript` / `typescript`) so the
    /// two languages never collide in `core_structural_nodes`.
    pub lang_tag: &'static str,
    pub src: &'s SourceText<'s>,
    /// Short names defined in this file — drives intra-file call matching.
    pub locals: Vec<String>,
    /// Local binding name → import specifier for imported bindings; calls to
    /// these become REFERENCES edges pointing at the module they came from.
    pub imported: Vec<(String, String)>,
}

/// Walk a program/class-body/function-body container.
/// `top_level` controls retrieval-unit segmentation marks.
pub(crate) fn walk_container(
    st: &mut JsState<'_>,
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
        walk_stmt(st, child, scope, parent_idx, top_level, out);
    }
}

/// Dispatch ONE statement/declaration node. Shared by the JS spec directly
/// and by language specs that extend ECMAScript (TypeScript).
pub(crate) fn walk_stmt(
    st: &mut JsState<'_>,
    child: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    {
        match child.kind() {
            "import_statement" => extract_import(child, st.src, out, st),
            "export_statement" => extract_export(st, child, top_level, out),
            "class_declaration" | "abstract_class_declaration" => {
                extract_class(st, child, scope, parent_idx, top_level, out)
            }
            "function_declaration" | "generator_function_declaration" => {
                extract_function_decl(st, child, scope, parent_idx, top_level, out)
            }
            "lexical_declaration" | "variable_declaration" => {
                extract_variable_decl(st, child, scope, parent_idx, top_level, out)
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Exports unwrap to their declarations (public visibility)
// ---------------------------------------------------------------------------

fn extract_export(st: &mut JsState<'_>, node: Node<'_>, top_level: bool, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    // `export ... from "<spec>"` / `export * from "<spec>"` → re-export edge.
    // Grammar 0.25 exposes the specifier as the `source` field.
    if let Some(src_node) = node.child_by_field_name("source")
        && let Some(spec) = last_string_fragment(src_node, st.src)
    {
        out.push_import(spec, "EXPORT_FROM", node);
        return;
    }

    for decl in named_children(node) {
        match decl.kind() {
            "class_declaration" | "abstract_class_declaration" => {
                extract_class_inner(st, decl, &[], None, top_level, true, out)
            }
            "function_declaration" | "generator_function_declaration" => {
                extract_function_decl_inner(st, decl, &[], None, top_level, true, out)
            }
            "lexical_declaration" | "variable_declaration" => {
                extract_variable_declarators(st, decl, &[], None, top_level, true, out)
            }
            _ => {}
        }
    }
}

fn is_directly_exported(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|p| p.kind() == "export_statement")
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

fn extract_class(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let exported = is_directly_exported(node);
    extract_class_inner(st, node, scope, parent_idx, top_level, exported, out);
}

pub(crate) fn extract_class_inner(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    is_public: bool,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);

    let mut qparts: Vec<String> = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");

    let identity = format!("{}|{qualified}|CLASS", st.lang_tag);
    let idx = out.push_node("CLASS", &name, node, identity, parent_idx);

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name);

    // extends X — syntactic fact only.
    let heritage = node.child_by_field_name("heritage").or_else(|| {
        named_children(node)
            .into_iter()
            .find(|c| c.kind() == "class_heritage")
    });
    if let Some(h) = heritage
        && let Some(base) = first_identifier_text(h, st.src)
    {
        out.push_rel(
            "EXTENDS",
            base,
            h,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }
    // TS implements clause (handled when walking TS trees through this path).
    // The clause nests inside `class_heritage` when extends is also present,
    // so search depth-first rather than scanning direct children only.
    if let Some(clause) = find_descendant(node, "implements_clause") {
        for target in implements_targets(clause) {
            out.push_rel(
                "IMPLEMENTS",
                text(target, st.src),
                clause,
                ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "method_definition" => {
                    extract_method_definition(st, member, &qparts, Some(idx), sym_idx, out)
                }
                "field_definition" | "public_field_definition" => {
                    extract_field_definition(st, member, &qparts, out)
                }
                _ => {}
            }
        }
    }
}

/// Target identifiers inside an `implements_clause`
/// (`type_identifier` or `generic_type` → inner `type_identifier`).
pub(crate) fn implements_targets(clause: Node<'_>) -> Vec<Node<'_>> {
    let mut found = Vec::new();
    let mut stack = vec![clause];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" | "identifier" => found.push(n),
            "generic_type" => {
                if let Some(name_t) = n.child_by_field_name("name") {
                    found.push(name_t);
                } else {
                    let mut c = n.walk();
                    for ch in n.children(&mut c) {
                        stack.push(ch);
                    }
                }
            }
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
    found
}

pub(crate) fn extract_method_definition(
    st: &mut JsState<'_>,
    node: Node<'_>,
    owner_qparts: &[String],
    owner_node: Option<usize>,
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();

    let qualified = format!("{}.{}", owner_qparts.join("."), name);
    let identity = format!("{}|{qualified}|METHOD|{params_text}", st.lang_tag);
    let idx = out.push_node(
        if name == "constructor" {
            "CONSTRUCTOR"
        } else {
            "METHOD"
        },
        &name,
        node,
        identity,
        owner_node,
    );

    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: !name.starts_with('#'),
        disambiguator: None,
        signature: Some(format!("{name}{params_text}")),
        visibility: Some(if name.starts_with('#') {
            "private".to_string()
        } else {
            "public".to_string()
        }),
        is_definition: node.child_by_field_name("body").is_some(),
        node_index: Some(idx),
    });
    st.locals.push(name);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls_in(st, body, Some(owner_sym), out);
    }
}

pub(crate) fn extract_field_definition(
    st: &mut JsState<'_>,
    node: Node<'_>,
    owner_qparts: &[String],
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node
        .child_by_field_name("property")
        .or_else(|| node.child_by_field_name("name"))
    else {
        return;
    };
    let raw = text(name_node, st.src);
    let private = raw.starts_with('#');
    let name = raw.trim_start_matches('#').to_string();
    let qualified = format!("{}.{}", owner_qparts.join("."), name);
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::Variable,
        span: span_of(node),
        is_public: !private,
        disambiguator: None,
        signature: None,
        visibility: Some(if private {
            "private".into()
        } else {
            "public".into()
        }),
        is_definition: true,
        node_index: None,
    });
}

// ---------------------------------------------------------------------------
// Functions
// ---------------------------------------------------------------------------

fn extract_function_decl(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let exported = is_directly_exported(node);
    extract_function_decl_inner(st, node, scope, parent_idx, top_level, exported, out);
}

pub(crate) fn extract_function_decl_inner(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    is_public: bool,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();

    let mut qparts: Vec<String> = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");
    let is_generator = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.kind() == "*");

    let identity = format!(
        "{lang}|{qualified}|FUNCTION|{params}{gen}",
        lang = st.lang_tag,
        params = params_text,
        gen = if is_generator { "|gen" } else { "" }
    );
    let idx = out.push_node("FUNCTION", &name, node, identity, parent_idx);

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: Some(format!("{name}{params_text}")),
        visibility: None,
        is_definition: node.child_by_field_name("body").is_some(),
        node_index: Some(idx),
    });
    st.locals.push(name);

    if top_level {
        out.mark_top_level(idx);
    }
    if let Some(body) = node.child_by_field_name("body") {
        collect_calls_in(st, body, Some(sym_idx), out);
        collect_nested(st, body, &qparts, Some(idx), out);
    }
}

fn collect_nested(
    st: &mut JsState<'_>,
    body: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    out: &mut Extraction<'_>,
) {
    for child in named_children(body) {
        if !out.tick() {
            return;
        }
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                extract_function_decl(st, child, scope, parent_idx, false, out)
            }
            "class_declaration" => extract_class(st, child, scope, parent_idx, false, out),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Variables / consts / arrows / require
// ---------------------------------------------------------------------------

fn extract_variable_decl(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let exported = is_directly_exported(node);
    extract_variable_declarators(st, node, scope, parent_idx, top_level, exported, out);
}

fn extract_variable_declarators(
    st: &mut JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    is_public: bool,
    out: &mut Extraction<'_>,
) {
    let _ = parent_idx;
    for declarator in named_children(node)
        .into_iter()
        .filter(|n| n.kind() == "variable_declarator")
    {
        if !out.tick() {
            return;
        }
        let Some(name_node) = declarator.child_by_field_name("name") else {
            continue;
        };
        if name_node.kind() != "identifier" {
            continue;
        }
        let name = text(name_node, st.src);
        let value = declarator.child_by_field_name("value");

        // require(...) / dynamic import(...) → import edge, not a symbol.
        if let Some(v) = value
            && v.kind() == "call_expression"
            && is_module_loader_call(v, st.src)
            && let Some(spec) = last_string_fragment(v, st.src)
        {
            let kind = loader_kind(v, st.src);
            out.push_import(spec, kind, node);
            continue;
        }

        let is_fn_like =
            value.is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"));

        if is_fn_like {
            let params = value.and_then(|v| v.child_by_field_name("parameters"));
            let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
            let mut qparts: Vec<String> = scope.to_vec();
            qparts.push(name.clone());
            let qualified = qparts.join(".");
            let identity = format!(
                "{lang}|{qualified}|FUNCTION_ARROW|{params}",
                lang = st.lang_tag,
                params = params_text
            );
            let idx = out.push_node("FUNCTION", &name, node, identity, None);
            let sym_idx = out.symbols.len();
            out.push_symbol(CanonSymbol {
                qualified_name: qualified,
                short_name: name.clone(),
                kind: SymbolKind::Function,
                span: span_of(node),
                is_public,
                disambiguator: None,
                signature: Some(format!("{name}{params_text}")),
                visibility: None,
                is_definition: true,
                node_index: Some(idx),
            });
            st.locals.push(name);
            if top_level {
                out.mark_top_level(idx);
            }
            if let Some(v) = value
                && let Some(body) = v.child_by_field_name("body")
            {
                collect_calls_in(st, body, Some(sym_idx), out);
            }
            continue;
        }

        let is_const = node
            .children(&mut node.walk())
            .any(|c| !c.is_named() && c.kind() == "const");
        if top_level && is_const && is_upper_const(&name) {
            let mut qparts: Vec<String> = scope.to_vec();
            qparts.push(name.clone());
            out.push_symbol(CanonSymbol {
                qualified_name: qparts.join("."),
                short_name: name.clone(),
                kind: SymbolKind::Constant,
                span: span_of(node),
                is_public,
                disambiguator: None,
                signature: None,
                visibility: Some("const".to_string()),
                is_definition: true,
                node_index: None,
            });
        }
        st.locals.push(name);
    }
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn extract_import(
    node: Node<'_>,
    src: &SourceText<'_>,
    out: &mut Extraction<'_>,
    st: &mut JsState<'_>,
) {
    let type_only = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.kind() == "type");
    let kind = if type_only { "IMPORT_TYPE" } else { "IMPORT" };
    if let Some(spec) = last_string_fragment(node, src) {
        out.push_import(spec.clone(), kind, node);
        // Record local bindings created by this import for call matching.
        // Shape: import_clause → (identifier | named_imports →
        // import_specifier | namespace_import).
        if let Some(clause) = named_children(node)
            .into_iter()
            .find(|n| n.kind() == "import_clause")
        {
            let record_binding = |st: &mut JsState<'_>, local: Node<'_>, spec: &str| {
                st.imported.push((text(local, src), spec.to_string()));
            };
            for ch in named_children(clause) {
                match ch.kind() {
                    "identifier" => record_binding(st, ch, &spec),
                    "namespace_import" => {
                        if let Some(n) = ch.child_by_field_name("name") {
                            record_binding(st, n, &spec);
                        }
                    }
                    "named_imports" => {
                        for sp in named_children(ch) {
                            if sp.kind() == "import_specifier" {
                                let local = sp
                                    .child_by_field_name("alias")
                                    .or_else(|| sp.child_by_field_name("name"));
                                if let Some(l) = local {
                                    record_binding(st, l, &spec);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Intra-file references
// ---------------------------------------------------------------------------

pub(crate) fn collect_calls_in(
    st: &mut JsState<'_>,
    node: Node<'_>,
    owner_sym: Option<usize>,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function")
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
                    return;
                }
                if let Some((_, specifier)) = st.imported.iter().find(|(name, _)| *name == callee) {
                    let target = specifier.clone();
                    // Reference to an imported binding — resolved only to the
                    // module it was imported from (syntactic evidence).
                    out.push_rel(
                        "REFERENCES",
                        target,
                        node,
                        ResolutionLevel::Syntactic,
                        0.7,
                        owner_sym,
                    );
                    return;
                }
            }
            recurse_calls(st, node, owner_sym, out);
        }
        "new_expression" => {
            if let Some(ctor) = node.child_by_field_name("constructor")
                && ctor.kind() == "identifier"
            {
                let callee = text(ctor, st.src);
                if st.locals.contains(&callee) {
                    out.push_rel(
                        "CALL",
                        callee,
                        node,
                        ResolutionLevel::SymbolResolved,
                        0.85,
                        owner_sym,
                    );
                    return;
                }
            }
            recurse_calls(st, node, owner_sym, out);
        }
        _ => recurse_calls(st, node, owner_sym, out),
    }
}

fn recurse_calls(
    st: &mut JsState<'_>,
    node: Node<'_>,
    owner_sym: Option<usize>,
    out: &mut Extraction<'_>,
) {
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        collect_calls_in(st, ch, owner_sym, out);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

pub(crate) fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn is_upper_const(s: &str) -> bool {
    s.chars().any(char::is_alphabetic)
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `require("x")` or dynamic `import("x")`.
fn is_module_loader_call(call: Node<'_>, src: &SourceText<'_>) -> bool {
    match call.child_by_field_name("function") {
        Some(f) if f.kind() == "identifier" => text(f, src) == "require",
        Some(f) if f.kind() == "import" => true,
        _ => false,
    }
}

fn loader_kind(call: Node<'_>, src: &SourceText<'_>) -> &'static str {
    match call.child_by_field_name("function") {
        Some(f) if f.kind() == "import" => "DYNAMIC",
        _ => {
            let _ = src;
            "REQUIRE"
        }
    }
}

/// Last `string_fragment` descendant (module specifiers).
pub(crate) fn last_string_fragment(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut last = None;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "string_fragment" {
            last = Some(text(n, src));
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    last
}

/// First descendant with `kind` (DFS, deterministic order).
pub(crate) fn find_descendant<'t>(root: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == kind && n != root {
            return Some(n);
        }
        let mut c = n.walk();
        let mut kids: Vec<Node<'_>> = n.children(&mut c).collect();
        kids.reverse();
        stack.extend(kids);
    }
    None
}

/// First `identifier`-kind descendant text (extends targets etc.).
pub(crate) fn first_identifier_text(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "identifier" => return Some(text(n, src)),
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
    None
}
