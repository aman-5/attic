//! C# language specification (grammar: `tree-sitter-c-sharp` 0.23.x).
//!
//! Grounded in the pinned grammar's parse tree: compilation units contain
//! `using_directive`, block and file-scoped namespaces, and type declarations;
//! type bodies expose classes/interfaces/structs/records/enums, methods,
//! constructors, properties and fields. Imports are extracted from `using`
//! directives, heritage from `base_list`, and intra-file references from
//! invocation/object-creation expressions.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static CSHARP_SPEC: CSharpSpec = CSharpSpec;

pub struct CSharpSpec;

pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&CSHARP_SPEC)
}

impl TreeSitterLanguageSpec for CSharpSpec {
    fn analyzer_id(&self) -> &'static str {
        "csharp-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for C#: symbols (namespaces, \
         classes/interfaces/structs/records/enums, methods, constructors, \
         properties, fields), using directives, heritage and intra-file calls."
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
        tree_sitter_c_sharp::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "csharp"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
        };
        if let Some(file_scoped) = named_children(root)
            .into_iter()
            .find(|child| child.kind() == "file_scoped_namespace_declaration")
        {
            let scope = file_scoped
                .child_by_field_name("name")
                .map(|name| namespace_parts(&text(name, src)))
                .unwrap_or_default();
            for child in named_children(root) {
                if !out.tick() {
                    return;
                }
                if child == file_scoped {
                    continue;
                }
                match child.kind() {
                    "using_directive" => extract_import(child, st.src, out),
                    "namespace_declaration" | "file_scoped_namespace_declaration" => {
                        extract_namespace(&mut st, child, &scope, None, true, out)
                    }
                    "class_declaration"
                    | "interface_declaration"
                    | "struct_declaration"
                    | "record_declaration"
                    | "record_struct_declaration"
                    | "enum_declaration" => {
                        extract_type_decl(&mut st, child, &scope, None, true, out)
                    }
                    "global_statement" => collect_calls(&mut st, child, None, out),
                    _ => {}
                }
            }
            return;
        }

        let scope: Vec<String> = Vec::new();
        extract_container_children(&mut st, root, &scope, None, true, out);
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
}

fn extract_container_children(
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
            "using_directive" => extract_import(child, st.src, out),
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                extract_namespace(st, child, scope, parent_idx, top_level, out);
            }
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "record_struct_declaration"
            | "enum_declaration" => extract_type_decl(st, child, scope, parent_idx, top_level, out),
            "global_statement" => collect_calls(st, child, None, out),
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
    let mut next_scope = scope.to_vec();
    next_scope.extend(namespace_parts(&text(name_node, st.src)));

    if let Some(body) = node.child_by_field_name("body") {
        extract_container_children(st, body, &next_scope, parent_idx, top_level, out);
        return;
    }

    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    for child in children {
        if !child.is_named() || child == name_node {
            continue;
        }
        if !out.tick() {
            return;
        }
        match child.kind() {
            "using_directive" => extract_import(child, st.src, out),
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                extract_namespace(st, child, &next_scope, parent_idx, top_level, out);
            }
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "record_struct_declaration"
            | "enum_declaration" => {
                extract_type_decl(st, child, &next_scope, parent_idx, top_level, out);
            }
            _ => {}
        }
    }
}

fn extract_type_decl(
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
    let (node_tag, kind, interface_like) = match node.kind() {
        "class_declaration" | "record_declaration" | "record_struct_declaration" => {
            ("CLASS", SymbolKind::Class, false)
        }
        "struct_declaration" => ("STRUCT", SymbolKind::Class, false),
        "interface_declaration" => ("INTERFACE", SymbolKind::Interface, true),
        "enum_declaration" => ("ENUM", SymbolKind::Class, false),
        _ => return,
    };

    let modifiers = modifier_texts(node, st.src);
    let qualified = qualify(scope, &name);
    let identity = format!("csharp|{qualified}|{node_tag}");
    let Some(idx) = out.push_node(node_tag, &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind,
        span: span_of(node),
        is_public: has_modifier(&modifiers, "public"),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(&modifiers),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    if node.kind() != "enum_declaration" {
        emit_heritage(st, node, sym_idx, interface_like, out);
    }

    if top_level {
        out.mark_top_level(idx);
    }

    if node.kind() == "enum_declaration" {
        if let Some(body) = node.child_by_field_name("body") {
            extract_enum_members(
                st,
                body,
                &qualified,
                idx,
                has_modifier(&modifiers, "public"),
                out,
            );
        }
        return;
    }

    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut next_scope = scope.to_vec();
    next_scope.push(name);
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration"
            | "record_struct_declaration"
            | "enum_declaration" => {
                extract_type_decl(st, member, &next_scope, Some(idx), false, out)
            }
            "method_declaration" => extract_method(
                st,
                member,
                &qualified,
                Some(idx),
                interface_like,
                false,
                out,
            ),
            "constructor_declaration" => {
                extract_method(st, member, &qualified, Some(idx), false, true, out)
            }
            "property_declaration" => {
                extract_property(st, member, &qualified, Some(idx), interface_like, out)
            }
            "field_declaration" => extract_fields(st, member, &qualified, out),
            _ => {}
        }
    }
}

fn emit_heritage(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_sym: usize,
    interface_like: bool,
    out: &mut Extraction<'_>,
) {
    let Some(base_list) = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "base_list")
    else {
        return;
    };

    let mut targets: Vec<Node<'_>> = named_children(base_list)
        .into_iter()
        .filter(|n| n.kind() != "argument_list")
        .collect();
    if targets.is_empty() {
        return;
    }

    if interface_like {
        for target in targets {
            if let Some(name) = base_name(&text(target, st.src)) {
                out.push_rel(
                    "EXTENDS",
                    name,
                    target,
                    ResolutionLevel::Syntactic,
                    0.5,
                    Some(owner_sym),
                );
            }
        }
        return;
    }

    let mut kind_iter = if matches!(
        node.kind(),
        "struct_declaration" | "record_struct_declaration"
    ) {
        None
    } else {
        Some("EXTENDS")
    };
    for target in targets.drain(..) {
        let rel_type = kind_iter.take().unwrap_or("IMPLEMENTS");
        if let Some(name) = base_name(&text(target, st.src)) {
            out.push_rel(
                rel_type,
                name,
                target,
                ResolutionLevel::Syntactic,
                0.5,
                Some(owner_sym),
            );
        }
    }
}

fn extract_method(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    default_public: bool,
    is_ctor: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let modifiers = modifier_texts(node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let returns_text = node
        .child_by_field_name("returns")
        .map(|r| format!(" -> {}", text(r, st.src)))
        .unwrap_or_default();
    let signature = format!("{name}{params_text}{returns_text}");

    let qualified = if let Some(interface_spec) = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "explicit_interface_specifier")
        .map(|n| text(n, st.src))
    {
        format!("{owner_qualified}.{interface_spec}.{name}")
    } else {
        format!("{owner_qualified}.{name}")
    };
    let Some(idx) = out.push_node(
        if is_ctor { "CONSTRUCTOR" } else { "METHOD" },
        &name,
        node,
        format!("csharp|{qualified}|METHOD|{signature}"),
        owner_node,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    let is_definition = node.child_by_field_name("body").is_some();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: has_modifier(&modifiers, "public")
            || (default_public && visibility_of(&modifiers).is_none()),
        disambiguator: None,
        signature: Some(signature),
        visibility: visibility_of(&modifiers),
        is_definition,
        node_index: Some(idx),
    });
    st.locals.push(name);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, Some(sym_idx), out);
    }
}

fn extract_property(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    default_public: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let ty = node
        .child_by_field_name("type")
        .map(|n| text(n, st.src))
        .unwrap_or_default();
    let modifiers = modifier_texts(node, st.src);
    let qualified = format!("{owner_qualified}.{name}");
    let Some(idx) = out.push_node(
        "PROPERTY",
        &name,
        node,
        format!("csharp|{qualified}|PROPERTY|{ty}"),
        owner_node,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::Variable,
        span: span_of(node),
        is_public: has_modifier(&modifiers, "public")
            || (default_public && visibility_of(&modifiers).is_none()),
        disambiguator: None,
        signature: (!ty.is_empty()).then_some(ty),
        visibility: visibility_of(&modifiers),
        is_definition: !default_public || property_is_definition(node),
        node_index: Some(idx),
    });

    if let Some(value) = node.child_by_field_name("value") {
        collect_calls(st, value, Some(sym_idx), out);
    }
    if let Some(accessors) = node.child_by_field_name("accessors") {
        collect_calls(st, accessors, Some(sym_idx), out);
    }
}

fn extract_fields(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    out: &mut Extraction<'_>,
) {
    let Some(decl) = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "variable_declaration")
    else {
        return;
    };
    let ty = decl
        .child_by_field_name("type")
        .map(|n| text(n, st.src))
        .unwrap_or_default();
    let modifiers = modifier_texts(node, st.src);
    let kind = if has_modifier(&modifiers, "const") {
        SymbolKind::Constant
    } else {
        SymbolKind::Variable
    };
    let is_public = has_modifier(&modifiers, "public");
    let visibility = visibility_of(&modifiers);

    for var in named_children(decl) {
        if var.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = var.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, st.src);
        out.push_symbol(CanonSymbol {
            qualified_name: format!("{owner_qualified}.{name}"),
            short_name: name,
            kind,
            span: span_of(var),
            is_public,
            disambiguator: None,
            signature: (!ty.is_empty()).then_some(ty.clone()),
            visibility: visibility.clone(),
            is_definition: true,
            node_index: None,
        });
    }
}

fn extract_enum_members(
    st: &mut St<'_>,
    body: Node<'_>,
    owner_qualified: &str,
    owner_idx: usize,
    is_public: bool,
    out: &mut Extraction<'_>,
) {
    for member in named_children(body) {
        if member.kind() != "enum_member_declaration" {
            continue;
        }
        let Some(name_node) = member.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, st.src);
        out.push_symbol(CanonSymbol {
            qualified_name: format!("{owner_qualified}.{name}"),
            short_name: name,
            kind: SymbolKind::Constant,
            span: span_of(member),
            is_public,
            disambiguator: None,
            signature: None,
            visibility: None,
            is_definition: true,
            node_index: Some(owner_idx),
        });
    }
}

fn extract_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let raw = text(node, src);
    let trimmed = raw.trim().trim_end_matches(';').trim();
    let rest = trimmed
        .strip_prefix("global ")
        .unwrap_or(trimmed)
        .trim_start();
    let Some(rest) = rest.strip_prefix("using ") else {
        return;
    };
    let rest = rest.trim_start();
    let (kind, rest) = if let Some(rest) = rest.strip_prefix("static ") {
        ("STATIC", rest.trim_start())
    } else {
        ("IMPORT", rest)
    };
    let specifier = if let Some((_, rhs)) = rest.split_once('=') {
        rhs.trim()
    } else {
        rest.trim()
    };
    if !specifier.is_empty() {
        let import_kind = if kind == "IMPORT" && rest.contains('=') {
            "ALIAS"
        } else {
            kind
        };
        out.push_import(specifier.to_string(), import_kind, node);
    }
}

fn collect_calls(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_sym: Option<usize>,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "invocation_expression" => {
            if let Some(func) = node.child_by_field_name("function")
                && let Some(callee) = base_name(&text(func, st.src))
                && st.locals.contains(&callee)
            {
                out.push_rel(
                    "CALL",
                    callee,
                    node,
                    ResolutionLevel::SymbolResolved,
                    0.85,
                    owner_sym,
                );
            }
        }
        "object_creation_expression" => {
            if let Some(ty) = node.child_by_field_name("type")
                && let Some(callee) = base_name(&text(ty, st.src))
                && st.locals.contains(&callee)
            {
                out.push_rel(
                    "CALL",
                    callee,
                    node,
                    ResolutionLevel::SymbolResolved,
                    0.85,
                    owner_sym,
                );
            }
        }
        _ => {}
    }

    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for child in kids {
        collect_calls(st, child, owner_sym, out);
    }
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn modifier_texts(node: Node<'_>, src: &SourceText<'_>) -> Vec<String> {
    named_children(node)
        .into_iter()
        .filter(|n| n.kind() == "modifier")
        .map(|n| text(n, src))
        .collect()
}

fn has_modifier(modifiers: &[String], kw: &str) -> bool {
    modifiers.iter().any(|m| m == kw)
}

fn visibility_of(modifiers: &[String]) -> Option<String> {
    ["public", "protected", "private", "internal"]
        .into_iter()
        .find(|kw| has_modifier(modifiers, kw))
        .map(str::to_string)
}

fn qualify(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", scope.join("."), name)
    }
}

fn namespace_parts(raw: &str) -> Vec<String> {
    raw.replace("::", ".")
        .split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != "global")
        .map(str::to_string)
        .collect()
}

fn base_name(raw: &str) -> Option<String> {
    let normalized = raw.replace("::", ".");
    let segment = normalized
        .rsplit('.')
        .find(|part| !part.trim().is_empty())?
        .trim();
    let truncated = segment
        .split(['<', '[', '(', '?', ' '])
        .next()
        .unwrap_or(segment)
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '@');
    let truncated = truncated.trim_start_matches('@');
    (!truncated.is_empty()).then(|| truncated.to_string())
}

fn property_is_definition(node: Node<'_>) -> bool {
    if node.child_by_field_name("value").is_some() {
        return true;
    }
    let Some(accessors) = node.child_by_field_name("accessors") else {
        return false;
    };
    named_children(accessors).into_iter().any(|accessor| {
        accessor.child_by_field_name("body").is_some()
            || accessor.child_by_field_name("value").is_some()
    })
}
