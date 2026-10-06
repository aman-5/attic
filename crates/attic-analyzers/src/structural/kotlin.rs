//! Kotlin language specification (grammar: `tree-sitter-kotlin-ng` 1.1.x).
//!
//! Grounded in parse trees produced by the pinned grammar. Notable observed
//! kinds:
//! `source_file`, `package_header`, `import`, `class_declaration`
//! (also used for `interface` / `data class` / `enum class` syntaxes),
//! `object_declaration`, `companion_object`, `primary_constructor` →
//! `class_parameters` → `class_parameter`, `delegation_specifiers` →
//! `delegation_specifier`, `function_declaration`, `property_declaration`,
//! `type_alias`, `call_expression`, `navigation_expression`,
//! `constructor_invocation`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static KOTLIN_SPEC: KotlinSpec = KotlinSpec;

pub struct KotlinSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&KOTLIN_SPEC)
}

impl TreeSitterLanguageSpec for KotlinSpec {
    fn analyzer_id(&self) -> &'static str {
        "kotlin-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Kotlin: structure, symbols \
         (classes/interfaces/objects, functions/methods, properties, type \
         aliases), imports, heritage via delegation specifiers, and intra-file \
         call edges."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Other]
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
        tree_sitter_kotlin_ng::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "kotlin"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            package: String::new(),
            locals: Vec::new(),
        };

        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "package_header" => st.package = parse_package(child, st.src),
                "import" => extract_import(child, st.src, out),
                _ => {}
            }
        }

        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "class_declaration" => extract_class_like(&mut st, child, &[], None, true, out),
                "object_declaration" => {
                    extract_object_like(&mut st, child, &[], None, true, ObjectFlavor::Object, out)
                }
                "companion_object" => extract_object_like(
                    &mut st,
                    child,
                    &[],
                    None,
                    true,
                    ObjectFlavor::Companion,
                    out,
                ),
                "function_declaration" => {
                    extract_function(&mut st, child, &[], None, true, false, out)
                }
                "property_declaration" => extract_property(&mut st, child, &[], true, out),
                "type_alias" => extract_type_alias(&mut st, child, &[], None, true, out),
                _ => {}
            }
        }
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    package: String,
    locals: Vec<String>,
}

enum TypeFlavor {
    Class,
    Interface,
}

enum ObjectFlavor {
    Object,
    Companion,
}

fn extract_class_like(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let raw = text(node, st.src);
    let flavor = if declaration_head(&raw).starts_with("interface ") {
        TypeFlavor::Interface
    } else {
        TypeFlavor::Class
    };
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let qualified = qname(st, scope, &name);
    let (node_tag, kind) = match flavor {
        TypeFlavor::Class => ("CLASS", SymbolKind::Class),
        TypeFlavor::Interface => ("INTERFACE", SymbolKind::Interface),
    };

    let identity = format!("kotlin|{qualified}|{node_tag}");
    let Some(idx) = out.push_node(node_tag, &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind,
        span: span_of(node),
        is_public: is_public(&raw),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(&raw),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    if top_level {
        out.mark_top_level(idx);
    }

    extract_constructor_properties(st, node, &qualified, out);
    extract_heritage(
        st,
        node,
        matches!(flavor, TypeFlavor::Interface),
        sym_idx,
        out,
    );

    let mut next_scope = scope.to_vec();
    next_scope.push(name);
    if let Some(body) = find_child_kind(node, "class_body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "function_declaration" => {
                    extract_function(st, member, &next_scope, Some(idx), false, true, out)
                }
                "property_declaration" => extract_property(st, member, &next_scope, false, out),
                "class_declaration" => {
                    extract_class_like(st, member, &next_scope, Some(idx), false, out)
                }
                "object_declaration" => extract_object_like(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    ObjectFlavor::Object,
                    out,
                ),
                "companion_object" => extract_object_like(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    ObjectFlavor::Companion,
                    out,
                ),
                _ => {}
            }
        }
    }
}

fn extract_object_like(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    flavor: ObjectFlavor,
    out: &mut Extraction<'_>,
) {
    let raw = text(node, st.src);
    let name = node
        .child_by_field_name("name")
        .map(|n| text(n, st.src))
        .or_else(|| matches!(flavor, ObjectFlavor::Companion).then_some(String::from("Companion")));
    let Some(name) = name else { return };
    let qualified = qname(st, scope, &name);
    let identity = format!("kotlin|{qualified}|OBJECT");
    let Some(idx) = out.push_node("OBJECT", &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: is_public(&raw),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(&raw),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    if top_level {
        out.mark_top_level(idx);
    }

    extract_heritage(st, node, false, sym_idx, out);

    let mut next_scope = scope.to_vec();
    next_scope.push(name);
    if let Some(body) = find_child_kind(node, "class_body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "function_declaration" => {
                    extract_function(st, member, &next_scope, Some(idx), false, true, out)
                }
                "property_declaration" => extract_property(st, member, &next_scope, false, out),
                "class_declaration" => {
                    extract_class_like(st, member, &next_scope, Some(idx), false, out)
                }
                "object_declaration" => extract_object_like(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    ObjectFlavor::Object,
                    out,
                ),
                "companion_object" => extract_object_like(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    ObjectFlavor::Companion,
                    out,
                ),
                _ => {}
            }
        }
    }
}

fn extract_constructor_properties(
    st: &mut St<'_>,
    node: Node<'_>,
    owner: &str,
    out: &mut Extraction<'_>,
) {
    let Some(ctor) = find_child_kind(node, "primary_constructor") else {
        return;
    };
    let Some(params) = find_child_kind(ctor, "class_parameters") else {
        return;
    };
    for param in named_children(params) {
        if !out.tick() {
            return;
        }
        if param.kind() != "class_parameter" {
            continue;
        }
        let raw = text(param, st.src);
        if !declares_property(&raw) {
            continue;
        }
        let Some(name_node) = named_children(param)
            .into_iter()
            .find(|n| n.kind() == "identifier")
        else {
            continue;
        };
        let name = text(name_node, st.src);
        out.push_symbol(CanonSymbol {
            qualified_name: format!("{owner}.{name}"),
            short_name: name.clone(),
            kind: SymbolKind::Variable,
            span: span_of(param),
            is_public: is_public(&raw),
            disambiguator: None,
            signature: Some(raw.clone()),
            visibility: visibility_of(&raw),
            is_definition: true,
            node_index: None,
        });
        st.locals.push(name);
    }
}

fn extract_function(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    is_method: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let raw = text(node, st.src);
    let params = find_child_kind(node, "function_value_parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let qualified = qname(st, scope, &name);
    let tag = if is_method { "METHOD" } else { "FUNCTION" };

    let Some(idx) = out.push_node(
        tag,
        &name,
        node,
        format!("kotlin|{qualified}|{tag}|{params_text}"),
        parent_idx,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        },
        span: span_of(node),
        is_public: is_public(&raw),
        disambiguator: None,
        signature: Some(format!("{name}{params_text}")),
        visibility: visibility_of(&raw),
        is_definition: find_child_kind(node, "function_body").is_some(),
        node_index: Some(idx),
    });
    st.locals.push(name);

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = find_child_kind(node, "function_body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_property(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    _top_level: bool,
    out: &mut Extraction<'_>,
) {
    let raw = text(node, st.src);
    let kind = if raw.contains("const val") || is_upper_const_name_from_decl(&raw) {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    for decl in named_children(node) {
        if !out.tick() {
            return;
        }
        if decl.kind() != "variable_declaration" {
            continue;
        }
        for ident in named_children(decl)
            .into_iter()
            .filter(|n| n.kind() == "identifier")
        {
            let name = text(ident, st.src);
            out.push_symbol(CanonSymbol {
                qualified_name: qname(st, scope, &name),
                short_name: name.clone(),
                kind,
                span: span_of(node),
                is_public: is_public(&raw),
                disambiguator: None,
                signature: Some(raw.clone()),
                visibility: visibility_of(&raw),
                is_definition: true,
                node_index: None,
            });
            st.locals.push(name);
        }
    }
}

fn extract_type_alias(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "identifier")
    else {
        return;
    };
    let name = text(name_node, st.src);
    let qualified = qname(st, scope, &name);
    let Some(idx) = out.push_node(
        "TYPE_ALIAS",
        &name,
        node,
        format!("kotlin|{qualified}|TYPE_ALIAS"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::TypeAlias,
        span: span_of(node),
        is_public: is_public(&text(node, st.src)),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(&text(node, st.src)),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name);
    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let raw = text(node, src);
    let spec = raw
        .trim()
        .strip_prefix("import")
        .unwrap_or(raw.trim())
        .trim();
    let spec = spec
        .split_once(" as ")
        .map(|(base, _)| base.trim())
        .unwrap_or(spec);
    if !spec.is_empty() {
        out.push_import(spec.to_string(), "IMPORT", node);
    }
}

fn extract_heritage(
    st: &mut St<'_>,
    node: Node<'_>,
    is_interface: bool,
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
    let Some(specs) = find_child_kind(node, "delegation_specifiers") else {
        return;
    };
    let mut seen_type = false;
    for spec in named_children(specs) {
        if !out.tick() {
            return;
        }
        if spec.kind() != "delegation_specifier" {
            continue;
        }
        let Some(target) = delegation_target(spec, st.src) else {
            continue;
        };
        let rel = if is_interface {
            "EXTENDS"
        } else {
            let is_explicit = spec_contains_child(spec, "explicit_delegation");
            if !seen_type && !is_explicit {
                seen_type = true;
                "EXTENDS"
            } else {
                "IMPLEMENTS"
            }
        };
        out.push_rel(
            rel,
            target,
            spec,
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
        "call_expression" => {
            let callee = named_children(node)
                .into_iter()
                .next()
                .and_then(|n| call_target_name(n, st.src));
            if let Some(callee) = callee
                && st.locals.contains(&callee)
            {
                out.push_rel(
                    "CALL",
                    callee,
                    node,
                    ResolutionLevel::SymbolResolved,
                    0.85,
                    Some(owner_sym),
                );
                return;
            }
            recurse_calls(st, node, owner_sym, out);
        }
        "function_declaration"
        | "class_declaration"
        | "object_declaration"
        | "companion_object" => {}
        _ => recurse_calls(st, node, owner_sym, out),
    }
}

fn recurse_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        collect_calls(st, ch, owner_sym, out);
    }
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn qname(st: &St<'_>, scope: &[String], name: &str) -> String {
    let mut parts = Vec::new();
    if !st.package.is_empty() {
        parts.push(st.package.clone());
    }
    parts.extend(scope.iter().cloned());
    parts.push(name.to_string());
    parts.join(".")
}

fn parse_package(node: Node<'_>, src: &SourceText<'_>) -> String {
    text(node, src)
        .trim()
        .strip_prefix("package")
        .unwrap_or("")
        .trim()
        .to_string()
}

fn find_child_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    named_children(node).into_iter().find(|n| n.kind() == kind)
}

fn declaration_head(raw: &str) -> &str {
    raw.trim_start()
}

fn visibility_of(raw: &str) -> Option<String> {
    ["public", "protected", "private", "internal"]
        .into_iter()
        .find(|kw| contains_word(raw, kw))
        .map(ToOwned::to_owned)
}

fn is_public(raw: &str) -> bool {
    !contains_word(raw, "private")
        && !contains_word(raw, "protected")
        && !contains_word(raw, "internal")
}

fn contains_word(raw: &str, needle: &str) -> bool {
    raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|part| part == needle)
}

fn declares_property(raw: &str) -> bool {
    contains_word(raw, "val") || contains_word(raw, "var")
}

fn is_upper_const_name_from_decl(raw: &str) -> bool {
    raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .find(|part| !part.is_empty() && *part != "val" && *part != "var")
        .is_some_and(is_upper_const_name)
}

fn is_upper_const_name(name: &str) -> bool {
    name.chars().any(char::is_alphabetic)
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn spec_contains_child(node: Node<'_>, kind: &str) -> bool {
    let mut stack = vec![node];
    while let Some(cur) = stack.pop() {
        if cur.kind() == kind {
            return true;
        }
        let mut c = cur.walk();
        for ch in cur.children(&mut c) {
            stack.push(ch);
        }
    }
    false
}

fn delegation_target(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(cur) = stack.pop() {
        if cur.kind() == "user_type" {
            return last_identifier_text(cur, src);
        }
        if cur.kind() == "constructor_invocation" {
            return last_identifier_text(cur, src);
        }
        let mut c = cur.walk();
        let kids: Vec<Node<'_>> = cur.children(&mut c).collect();
        for ch in kids.into_iter().rev() {
            stack.push(ch);
        }
    }
    None
}

fn call_target_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    match node.kind() {
        "identifier" => Some(text(node, src)),
        "navigation_expression" => last_identifier_text(node, src),
        _ => None,
    }
}

fn last_identifier_text(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    if node.kind() == "identifier" {
        return Some(text(node, src));
    }
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids.into_iter().rev() {
        if let Some(found) = last_identifier_text(ch, src) {
            return Some(found);
        }
    }
    None
}
