//! PHP language specification (grammar: `tree-sitter-php` 0.24.x).
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `program`, `namespace_definition`, `namespace_use_declaration`
//! (`namespace_use_clause`, `namespace_use_group`, `qualified_name`),
//! `require*_expression`, `include*_expression`, `class_declaration`,
//! `interface_declaration`, `trait_declaration`, `enum_declaration`,
//! `function_definition`, `method_declaration`, `use_declaration`,
//! `function_call_expression`, `member_call_expression`,
//! `scoped_call_expression`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static PHP_SPEC: PhpSpec = PhpSpec;

pub struct PhpSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&PHP_SPEC)
}

impl TreeSitterLanguageSpec for PhpSpec {
    fn analyzer_id(&self) -> &'static str {
        "php-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for PHP: structure, symbols \
         (namespaces, classes, interfaces, traits, enums, functions, \
         methods), namespace imports, include/require edges, heritage and \
         intra-file call relationships."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[]
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
        tree_sitter_php::LANGUAGE_PHP
    }

    fn language_tag(&self) -> &'static str {
        "php"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
            namespace: String::new(),
        };
        walk_container(&mut st, root, &[], None, true, out);
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
    namespace: String,
}

fn walk_container(
    st: &mut St<'_>,
    container: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    prime_local_names(st, container);
    for child in named_children(container) {
        if !out.tick() {
            return;
        }
        match child.kind() {
            "namespace_definition" => {
                extract_namespace(st, child, scope, parent_idx, top_level, out)
            }
            "namespace_use_declaration" => extract_namespace_use(st, child, out),
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => extract_type(st, child, scope, parent_idx, top_level, out),
            "function_definition" => extract_function(st, child, scope, parent_idx, top_level, out),
            "expression_statement" => {
                let _ = extract_file_import(child, st.src, out);
            }
            _ => {}
        }
    }
}

fn extract_namespace(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let declared = normalize_namespace(&text(name_node, st.src));
    let short = last_segment(&declared);
    let identity = format!("php|{declared}|NAMESPACE");
    let idx = out.push_node("NAMESPACE", &short, node, identity, parent_idx);
    if let Some(idx) = idx {
        out.push_symbol(CanonSymbol {
            qualified_name: declared.clone(),
            short_name: short,
            kind: SymbolKind::Module,
            span: span_of(node),
            is_public: true,
            disambiguator: None,
            signature: None,
            visibility: None,
            is_definition: true,
            node_index: Some(idx),
        });
        if top_level {
            out.mark_top_level(idx);
        }
    }

    let previous = st.namespace.clone();
    st.namespace = declared;
    if let Some(body) = node.child_by_field_name("body") {
        walk_container(st, body, scope, parent_idx, false, out);
        st.namespace = previous;
    }
}

fn extract_type(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let qualified = qname(st, scope, &name);
    let (tag, kind) = match node.kind() {
        "class_declaration" => ("CLASS", SymbolKind::Class),
        "interface_declaration" => ("INTERFACE", SymbolKind::Interface),
        "trait_declaration" => ("TRAIT", SymbolKind::Interface),
        "enum_declaration" => ("ENUM", SymbolKind::Class),
        _ => return,
    };
    let identity = format!("php|{qualified}|{tag}");
    let Some(idx) = out.push_node(tag, &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(base_clause) = named_children(node)
        .into_iter()
        .find(|child| child.kind() == "base_clause")
        && let Some(target) = first_name_like(base_clause, st.src)
    {
        out.push_rel(
            "EXTENDS",
            target,
            base_clause,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }
    if let Some(interface_clause) = named_children(node)
        .into_iter()
        .find(|child| child.kind() == "class_interface_clause")
    {
        for target in name_like_targets(interface_clause, st.src) {
            out.push_rel(
                "IMPLEMENTS",
                target,
                interface_clause,
                ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut next_scope = scope.to_vec();
        next_scope.push(name);
        walk_type_body(st, body, &next_scope, &qualified, idx, sym_idx, out);
    }
}

fn walk_type_body(
    st: &mut St<'_>,
    body: Node<'_>,
    scope: &[String],
    owner_qualified: &str,
    owner_idx: usize,
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
    prime_local_names(st, body);
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "method_declaration" => {
                extract_method(st, member, scope, owner_qualified, owner_idx, out)
            }
            "use_declaration" => extract_trait_use(st, member, owner_sym, out),
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => extract_type(st, member, scope, Some(owner_idx), false, out),
            _ => {}
        }
    }
}

fn extract_function(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let qualified = qname(st, scope, &name);
    let params = node
        .child_by_field_name("parameters")
        .map(|params| text(params, st.src))
        .unwrap_or_default();
    let identity = format!("php|{qualified}|FUNCTION|{params}");
    let Some(idx) = out.push_node("FUNCTION", &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(format!("{name}{params}")),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_method(
    st: &mut St<'_>,
    node: Node<'_>,
    _scope: &[String],
    owner_qualified: &str,
    owner_idx: usize,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node
        .child_by_field_name("parameters")
        .map(|params| text(params, st.src))
        .unwrap_or_default();
    let visibility = visibility_of(node, st.src);
    let has_body = node.child_by_field_name("body").is_some();
    let qualified = format!("{owner_qualified}.{name}");
    let identity = format!("php|{qualified}|METHOD|{params}");
    let Some(idx) = out.push_node("METHOD", &name, node, identity, Some(owner_idx)) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: !matches!(visibility.as_deref(), Some("private" | "protected")),
        disambiguator: None,
        signature: Some(format!("{name}{params}")),
        visibility,
        is_definition: has_body,
        node_index: Some(idx),
    });

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_namespace_use(st: &St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let prefix = named_children(node)
        .into_iter()
        .find(|child| child.kind() == "namespace_name")
        .map(|child| text(child, st.src));
    let group_body = node.child_by_field_name("body");

    for clause in named_children(node)
        .into_iter()
        .chain(group_body.into_iter().flat_map(named_children))
        .filter(|child| child.kind() == "namespace_use_clause")
    {
        let target = clause
            .child_by_field_name("name")
            .map(|name| text(name, st.src))
            .or_else(|| {
                named_children(clause)
                    .into_iter()
                    .find(|child| child.kind() == "qualified_name" || child.kind() == "name")
                    .map(|child| text(child, st.src))
            });
        let Some(target) = target else {
            continue;
        };
        let raw = if let Some(prefix) = &prefix {
            if group_body.is_some() && !target.contains('\\') {
                format!("{prefix}\\{target}")
            } else {
                target
            }
        } else {
            target
        };
        out.push_import(raw, "USE", node);
    }
}

fn extract_file_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) -> bool {
    let Some(expr) = named_children(node).into_iter().next() else {
        return false;
    };
    let kind = match expr.kind() {
        "require_expression" => "REQUIRE",
        "require_once_expression" => "REQUIRE_ONCE",
        "include_expression" => "INCLUDE",
        "include_once_expression" => "INCLUDE_ONCE",
        _ => return false,
    };
    let Some(spec) = first_string_literal(expr, src) else {
        return false;
    };
    out.push_import(spec, kind, expr);
    true
}

fn extract_trait_use(st: &St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    for target in name_like_targets(node, st.src) {
        out.push_rel(
            "IMPLEMENTS",
            target,
            node,
            ResolutionLevel::Syntactic,
            0.5,
            Some(owner_sym),
        );
    }
}

fn collect_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "function_call_expression" => {
            if let Some(function) = node.child_by_field_name("function") {
                let target = normalize_namespace(&text(function, st.src));
                let short = last_segment(&target);
                let (target, resolution, confidence) = if st.locals.contains(&short) {
                    (short, ResolutionLevel::SymbolResolved, 0.85)
                } else {
                    (target, ResolutionLevel::Syntactic, 0.65)
                };
                out.push_rel(
                    "CALL",
                    target,
                    node,
                    resolution,
                    confidence,
                    Some(owner_sym),
                );
            }
        }
        "member_call_expression" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return recurse_calls(st, node, owner_sym, out);
            };
            let name = text(name_node, st.src);
            let object = node.child_by_field_name("object");
            let (target, resolution, confidence) = if object
                .is_some_and(|obj| text(obj, st.src) == "$this")
                && st.locals.contains(&name)
            {
                (name.clone(), ResolutionLevel::SymbolResolved, 0.8)
            } else {
                (name.clone(), ResolutionLevel::Syntactic, 0.6)
            };
            out.push_rel(
                "CALL",
                target,
                node,
                resolution,
                confidence,
                Some(owner_sym),
            );
        }
        "scoped_call_expression" => {
            let Some(name_node) = node.child_by_field_name("name") else {
                return recurse_calls(st, node, owner_sym, out);
            };
            let name = text(name_node, st.src);
            let scope = node
                .child_by_field_name("scope")
                .map(|scope| normalize_namespace(&text(scope, st.src)))
                .unwrap_or_default();
            let (target, resolution, confidence) = if st.locals.contains(&name) {
                (name.clone(), ResolutionLevel::SymbolResolved, 0.8)
            } else {
                (format!("{scope}.{name}"), ResolutionLevel::Syntactic, 0.7)
            };
            out.push_rel(
                "CALL",
                target,
                node,
                resolution,
                confidence,
                Some(owner_sym),
            );
        }
        _ => {}
    }
    recurse_calls(st, node, owner_sym, out);
}

fn recurse_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for child in kids {
        collect_calls(st, child, owner_sym, out);
    }
}

fn prime_local_names(st: &mut St<'_>, container: Node<'_>) {
    for child in named_children(container) {
        let candidate = match child.kind() {
            "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration"
            | "function_definition"
            | "method_declaration" => child
                .child_by_field_name("name")
                .map(|name| text(name, st.src)),
            _ => None,
        };
        if let Some(name) = candidate
            && !st.locals.contains(&name)
        {
            st.locals.push(name);
        }
    }
}

fn first_string_literal(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        if next.kind() == "encapsed_string" {
            return named_children(next)
                .into_iter()
                .find(|child| child.kind() == "string_content")
                .map(|child| text(child, src));
        }
        let mut c = next.walk();
        for child in next.children(&mut c) {
            stack.push(child);
        }
    }
    None
}

fn first_name_like(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut targets = name_like_targets(node, src);
    targets.drain(..).next()
}

fn name_like_targets(node: Node<'_>, src: &SourceText<'_>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        match next.kind() {
            "name" | "qualified_name" => out.push(normalize_namespace(&text(next, src))),
            _ => {
                let mut c = next.walk();
                for child in next.children(&mut c) {
                    stack.push(child);
                }
            }
        }
    }
    out.reverse();
    out
}

fn visibility_of(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    named_children(node)
        .into_iter()
        .find(|child| child.kind() == "visibility_modifier")
        .map(|child| text(child, src))
}

fn qname(st: &St<'_>, scope: &[String], name: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !st.namespace.is_empty() {
        parts.extend(st.namespace.split('.').map(ToOwned::to_owned));
    }
    parts.extend(scope.iter().cloned());
    parts.push(name.to_string());
    parts.join(".")
}

fn last_segment(path: &str) -> String {
    path.rsplit('.').next().unwrap_or(path).to_string()
}

fn normalize_namespace(raw: &str) -> String {
    raw.trim_start_matches('\\').replace('\\', ".")
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c)
        .filter(|child| child.is_named())
        .collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}
