//! Ruby language specification (grammar: `tree-sitter-ruby` 0.23.x).
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `program`, `module`, `class` (`superclass`), `method`, `singleton_method`,
//! `assignment` (`left` constant), `call` (imports, mixins, dynamic calls),
//! `constant`, `identifier`, `self`, `argument_list`, `string`,
//! `string_content`, `simple_symbol`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static RUBY_SPEC: RubySpec = RubySpec;

pub struct RubySpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&RUBY_SPEC)
}

impl TreeSitterLanguageSpec for RubySpec {
    fn analyzer_id(&self) -> &'static str {
        "ruby-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Ruby: structure, symbols \
         (classes, modules, methods, singleton methods, constants), import \
         forms (`require`, `require_relative`, `load`, `autoload`), mixins \
         and name-based call edges."
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
        tree_sitter_ruby::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "ruby"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
        };
        walk_container(&mut st, root, &[], None, true, out);
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
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
            "module" => extract_module(st, child, scope, parent_idx, top_level, out),
            "class" => extract_class(st, child, scope, parent_idx, top_level, out),
            "method" => extract_method(st, child, None, scope, parent_idx, top_level, false, out),
            "singleton_method" => {
                extract_singleton_method(st, child, None, scope, parent_idx, top_level, out)
            }
            "assignment" => extract_constant_assignment(st, child, None, scope, out),
            "call" => {
                let _ = extract_import_call(child, st.src, out);
            }
            _ => {}
        }
    }
}

fn extract_module(
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
    let declared = normalize_const_path(&text(name_node, st.src));
    let short = last_segment(&declared);
    let qualified = qname(scope, &declared);
    let identity = format!("ruby|{qualified}|MODULE");
    let Some(idx) = out.push_node("MODULE", &short, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: short.clone(),
        kind: SymbolKind::Module,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(short.clone());
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut next_scope = scope.to_vec();
        next_scope.push(short);
        walk_type_body(st, body, &next_scope, &qualified, idx, sym_idx, out);
    }
}

fn extract_class(
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
    let declared = normalize_const_path(&text(name_node, st.src));
    let short = last_segment(&declared);
    let qualified = qname(scope, &declared);
    let identity = format!("ruby|{qualified}|CLASS");
    let Some(idx) = out.push_node("CLASS", &short, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: short.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(short.clone());
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(superclass) = node.child_by_field_name("superclass")
        && let Some(target) = first_const_descendant(superclass, st.src)
    {
        out.push_rel(
            "EXTENDS",
            target,
            superclass,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut next_scope = scope.to_vec();
        next_scope.push(short);
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
            "module" => extract_module(st, member, scope, Some(owner_idx), false, out),
            "class" => extract_class(st, member, scope, Some(owner_idx), false, out),
            "method" => extract_method(
                st,
                member,
                Some(owner_qualified),
                scope,
                Some(owner_idx),
                false,
                true,
                out,
            ),
            "singleton_method" => extract_singleton_method(
                st,
                member,
                Some(owner_qualified),
                scope,
                Some(owner_idx),
                false,
                out,
            ),
            "assignment" => {
                extract_constant_assignment(st, member, Some(owner_qualified), scope, out)
            }
            "call" if !extract_import_call(member, st.src, out) => {
                extract_mixin_call(st, member, owner_sym, out);
            }
            "call" => {}
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_method(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: Option<&str>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    is_member: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let qualified = match owner_qualified {
        Some(owner) => format!("{owner}.{name}"),
        None => qname(scope, &name),
    };
    let tag = if is_member { "METHOD" } else { "FUNCTION" };
    let identity = format!("ruby|{qualified}|{tag}|{params_text}");
    let Some(idx) = out.push_node(tag, &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: if is_member {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        },
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(format!("{name}{params_text}")),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(method_body) = node.child_by_field_name("body") {
        collect_calls(st, method_body, sym_idx, out);
    }
}

fn extract_singleton_method(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: Option<&str>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Some(object_node) = node.child_by_field_name("object") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let qualified = if object_node.kind() == "self" {
        owner_qualified
            .map(|owner| format!("{owner}.{name}"))
            .unwrap_or_else(|| qname(scope, &name))
    } else {
        let base = normalize_const_path(&text(object_node, st.src));
        format!("{}.{}", qname(scope, &base), name)
    };
    let identity = format!("ruby|{qualified}|SINGLETON_METHOD|{params_text}");
    let Some(idx) = out.push_node("SINGLETON_METHOD", &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(format!("{name}{params_text}")),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(method_body) = node.child_by_field_name("body") {
        collect_calls(st, method_body, sym_idx, out);
    }
}

fn extract_constant_assignment(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: Option<&str>,
    scope: &[String],
    out: &mut Extraction<'_>,
) {
    let Some(left) = node.child_by_field_name("left") else {
        return;
    };
    if left.kind() != "constant" {
        return;
    }
    let declared = normalize_const_path(&text(left, st.src));
    let short = last_segment(&declared);
    let qualified = owner_qualified
        .map(|owner| format!("{owner}.{short}"))
        .unwrap_or_else(|| qname(scope, &declared));
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: short.clone(),
        kind: SymbolKind::Constant,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: None,
    });
    st.locals.push(short);
}

fn extract_import_call(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) -> bool {
    if node.kind() != "call" || node.child_by_field_name("receiver").is_some() {
        return false;
    }
    let Some(method) = node.child_by_field_name("method") else {
        return false;
    };
    let method_name = text(method, src);
    match method_name.as_str() {
        "require" => string_arg(node, src, 0).is_some_and(|spec| {
            out.push_import(spec, "REQUIRE", node);
            true
        }),
        "require_relative" => string_arg(node, src, 0).is_some_and(|spec| {
            out.push_import(spec, "REQUIRE_RELATIVE", node);
            true
        }),
        "load" => string_arg(node, src, 0).is_some_and(|spec| {
            out.push_import(spec, "LOAD", node);
            true
        }),
        "autoload" => string_arg(node, src, 1).is_some_and(|spec| {
            out.push_import(spec, "AUTOLOAD", node);
            true
        }),
        _ => false,
    }
}

fn extract_mixin_call(st: &St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    let Some(method) = node.child_by_field_name("method") else {
        return;
    };
    let rel_type = match text(method, st.src).as_str() {
        "include" | "extend" | "prepend" => "IMPLEMENTS",
        _ => return,
    };
    let Some(args) = node.child_by_field_name("arguments") else {
        return;
    };
    for arg in named_children(args) {
        let target = match arg.kind() {
            "constant" => Some(normalize_const_path(&text(arg, st.src))),
            _ => None,
        };
        if let Some(target) = target {
            out.push_rel(
                rel_type,
                target,
                arg,
                ResolutionLevel::Syntactic,
                0.5,
                Some(owner_sym),
            );
        }
    }
}

fn collect_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    if node.kind() == "call" {
        if extract_import_call(node, st.src, out) {
            return;
        }
        if let Some(method) = node.child_by_field_name("method") {
            let name = text(method, st.src);
            let receiver = node
                .child_by_field_name("receiver")
                .map(|recv| normalize_const_path(&text(recv, st.src)));
            let (target, resolution, confidence) = if st.locals.contains(&name) {
                (name.clone(), ResolutionLevel::SymbolResolved, 0.85)
            } else if let Some(receiver) = receiver {
                (
                    format!("{receiver}.{name}"),
                    ResolutionLevel::Syntactic,
                    0.7,
                )
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
    }
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for child in kids {
        collect_calls(st, child, owner_sym, out);
    }
}

fn string_arg(node: Node<'_>, src: &SourceText<'_>, index: usize) -> Option<String> {
    let args = node.child_by_field_name("arguments")?;
    named_children(args)
        .into_iter()
        .nth(index)
        .and_then(|arg| match arg.kind() {
            "string" => string_content(arg, src),
            _ => None,
        })
}

fn string_content(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    named_children(node)
        .into_iter()
        .find(|child| child.kind() == "string_content")
        .map(|child| text(child, src))
}

fn first_const_descendant(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        match next.kind() {
            "constant" => return Some(normalize_const_path(&text(next, src))),
            _ => {
                let mut c = next.walk();
                for child in next.children(&mut c) {
                    stack.push(child);
                }
            }
        }
    }
    None
}

fn prime_local_names(st: &mut St<'_>, container: Node<'_>) {
    for child in named_children(container) {
        let candidate = match child.kind() {
            "module" | "class" => child
                .child_by_field_name("name")
                .map(|name| last_segment(&normalize_const_path(&text(name, st.src)))),
            "method" | "singleton_method" => child
                .child_by_field_name("name")
                .map(|name| text(name, st.src)),
            "assignment" => child.child_by_field_name("left").and_then(|left| {
                (left.kind() == "constant")
                    .then(|| last_segment(&normalize_const_path(&text(left, st.src))))
            }),
            _ => None,
        };
        if let Some(name) = candidate
            && !st.locals.contains(&name)
        {
            st.locals.push(name);
        }
    }
}

fn qname(scope: &[String], declared: &str) -> String {
    if scope.is_empty() {
        declared.to_string()
    } else {
        format!("{}.{}", scope.join("."), declared)
    }
}

fn last_segment(path: &str) -> String {
    path.rsplit('.').next().unwrap_or(path).to_string()
}

fn normalize_const_path(raw: &str) -> String {
    raw.trim_start_matches("::").replace("::", ".")
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
