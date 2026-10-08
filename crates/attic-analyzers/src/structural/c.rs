//! C language specification (grammar: `tree-sitter-c` 0.24.x).
//!
//! Extraction is intentionally syntax-driven and tolerant: malformed source
//! and preprocessor-heavy files still produce partial structure instead of a
//! hard failure. We record top-level symbols, `#include` edges, macros and
//! intra-file call relationships without evaluating preprocessor branches.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerDiagnostic, CapabilityKind, CapabilityLevel,
    ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static C_SPEC: CSpec = CSpec;

pub struct CSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&C_SPEC)
}

impl TreeSitterLanguageSpec for CSpec {
    fn analyzer_id(&self) -> &'static str {
        "c-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for C: functions and prototypes, \
         structs/unions/enums/typedefs, global variables, macros, #include \
         edges, and intra-file call relationships without preprocessor \
         branch evaluation."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::C]
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
        tree_sitter_c::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "c"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st: St<'_, '_> = St {
            src,
            callables: Vec::new(),
            pending_calls: Vec::new(),
            saw_conditional: false,
        };
        walk_top_level(&mut st, root, out);
        for (body, owner_sym) in st.pending_calls.iter().copied() {
            if !out.tick() {
                return;
            }
            collect_calls(body, st.src, &st.callables, owner_sym, out);
        }
    }
}

struct St<'tree, 'src> {
    src: &'src SourceText<'src>,
    callables: Vec<String>,
    pending_calls: Vec<(Node<'tree>, usize)>,
    saw_conditional: bool,
}

#[derive(Clone)]
struct DeclInfo {
    short_name: String,
    signature: Option<String>,
    is_function: bool,
}

fn walk_top_level<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    out: &mut Extraction<'out>,
) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "translation_unit" => {
            for child in named_children(node) {
                walk_top_level(st, child, out);
                if !out.tick() {
                    return;
                }
            }
        }
        "preproc_include" => extract_include(node, st.src, out),
        "preproc_def" | "preproc_function_def" => extract_macro(node, st.src, out),
        "function_definition" => extract_function_definition(st, node, out),
        "declaration" => extract_declaration(st, node, out),
        "type_definition" => extract_type_definition(st, node, out),
        "struct_specifier" | "union_specifier" | "enum_specifier" => {
            if specifier_has_body(node) {
                emit_tagged_type(node, node, st.src, out, true);
            }
        }
        kind if is_branching_preproc(kind) => {
            mark_preprocessor_partial(st, out);
            for child in named_children(node) {
                walk_top_level(st, child, out);
                if !out.tick() {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn extract_include(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let raw = text(node, src);
    if let Some((specifier, kind)) = parse_include(&raw) {
        out.push_import(specifier, kind, node);
    }
}

fn extract_macro(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| named_child(node, "identifier"))
    else {
        return;
    };
    let name = text(name_node, src);
    let Some(idx) = out.push_node("MACRO", &name, node, format!("c|{name}|MACRO"), None) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Macro,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(text(node, src).trim().to_string()),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    out.mark_top_level(idx);
}

fn extract_type_definition<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    out: &mut Extraction<'out>,
) {
    let mut emitted_tagged = false;
    for child in named_children(node) {
        if !out.tick() {
            return;
        }
        if matches!(
            child.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        ) && specifier_has_body(child)
        {
            emit_tagged_type(node, child, st.src, out, true);
            emitted_tagged = true;
        }
    }

    for decl in declaration_declarators(node) {
        if !out.tick() {
            return;
        }
        let Some(info) = declarator_info(decl, st.src) else {
            continue;
        };
        let Some(idx) = out.push_node(
            "TYPE_ALIAS",
            &info.short_name,
            decl,
            format!("c|{}|TYPE_ALIAS", info.short_name),
            None,
        ) else {
            return;
        };
        out.push_symbol(CanonSymbol {
            qualified_name: info.short_name.clone(),
            short_name: info.short_name,
            kind: SymbolKind::TypeAlias,
            span: span_of(decl),
            is_public: true,
            disambiguator: None,
            signature: None,
            visibility: None,
            is_definition: true,
            node_index: Some(idx),
        });
        if !emitted_tagged {
            out.mark_top_level(idx);
        }
    }
}

fn extract_declaration<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    out: &mut Extraction<'out>,
) {
    let is_static = has_storage_class(node, st.src, "static");
    let is_extern = has_storage_class(node, st.src, "extern");
    let visibility = if is_static {
        Some("static".to_string())
    } else if is_extern {
        Some("extern".to_string())
    } else {
        None
    };

    for child in named_children(node) {
        if !out.tick() {
            return;
        }
        if matches!(
            child.kind(),
            "struct_specifier" | "union_specifier" | "enum_specifier"
        ) && specifier_has_body(child)
        {
            emit_tagged_type(child, child, st.src, out, true);
        }
    }

    for decl in declaration_declarators(node) {
        if !out.tick() {
            return;
        }
        let Some(info) = declarator_info(decl, st.src) else {
            continue;
        };
        if info.is_function {
            let Some(idx) = out.push_node(
                "FUNCTION_DECL",
                &info.short_name,
                decl,
                format!(
                    "c|{}|FUNCTION_DECL|{}",
                    info.short_name,
                    info.signature.clone().unwrap_or_default()
                ),
                None,
            ) else {
                return;
            };
            out.push_symbol(CanonSymbol {
                qualified_name: info.short_name.clone(),
                short_name: info.short_name.clone(),
                kind: SymbolKind::Function,
                span: span_of(decl),
                is_public: !is_static,
                disambiguator: None,
                signature: info.signature,
                visibility: visibility.clone(),
                is_definition: false,
                node_index: Some(idx),
            });
            remember_callable(&mut st.callables, info.short_name);
            out.mark_top_level(idx);
        } else {
            out.push_symbol(CanonSymbol {
                qualified_name: info.short_name.clone(),
                short_name: info.short_name,
                kind: SymbolKind::Variable,
                span: span_of(decl),
                is_public: !is_static,
                disambiguator: None,
                signature: None,
                visibility: visibility.clone(),
                is_definition: !is_extern,
                node_index: None,
            });
        }
    }
}

fn extract_function_definition<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    out: &mut Extraction<'out>,
) {
    let Some(decl) = node.child_by_field_name("declarator").or_else(|| {
        named_children(node)
            .into_iter()
            .find(|child| is_declarator_node(*child))
    }) else {
        return;
    };
    let Some(info) = declarator_info(decl, st.src) else {
        return;
    };
    let is_static = has_storage_class(node, st.src, "static");
    let visibility = is_static.then(|| "static".to_string());

    let Some(idx) = out.push_node(
        "FUNCTION",
        &info.short_name,
        node,
        format!(
            "c|{}|FUNCTION|{}",
            info.short_name,
            info.signature.clone().unwrap_or_default()
        ),
        None,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: info.short_name.clone(),
        short_name: info.short_name.clone(),
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public: !is_static,
        disambiguator: None,
        signature: info.signature,
        visibility,
        is_definition: true,
        node_index: Some(idx),
    });
    remember_callable(&mut st.callables, info.short_name);
    out.mark_top_level(idx);

    if let Some(body) = node.child_by_field_name("body") {
        st.pending_calls.push((body, sym_idx));
    }
}

fn emit_tagged_type(
    span_node: Node<'_>,
    spec_node: Node<'_>,
    src: &SourceText<'_>,
    out: &mut Extraction<'_>,
    top_level: bool,
) {
    let Some(name) = named_type_name(spec_node, src) else {
        return;
    };
    let node_type = match spec_node.kind() {
        "struct_specifier" => "STRUCT",
        "union_specifier" => "UNION",
        "enum_specifier" => "ENUM",
        _ => return,
    };
    let Some(idx) = out.push_node(
        node_type,
        &name,
        span_node,
        format!("c|{name}|{node_type}"),
        None,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Class,
        span: span_of(span_node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: specifier_has_body(spec_node),
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }
}

fn collect_calls(
    node: Node<'_>,
    src: &SourceText<'_>,
    callables: &[String],
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && let Some(callee) = last_name(func, src)
        && callables.iter().any(|known| known == &callee)
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
    let mut cursor = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut cursor).collect();
    for child in kids {
        collect_calls(child, src, callables, owner_sym, out);
    }
}

fn mark_preprocessor_partial(st: &mut St<'_, '_>, out: &mut Extraction<'_>) {
    if st.saw_conditional {
        return;
    }
    st.saw_conditional = true;
    if !out.truncations.contains(&"preprocessor") {
        out.truncations.push("preprocessor");
    }
    out.diagnostics.push(AnalyzerDiagnostic::warning(
        "PREPROCESSOR_PARTIAL",
        "Preprocessor conditionals were indexed from every branch without \
         evaluation; structural output is PARTIAL.",
    ));
}

fn remember_callable(callables: &mut Vec<String>, name: String) {
    if !callables.iter().any(|known| known == &name) {
        callables.push(name);
    }
}

fn parse_include(raw: &str) -> Option<(String, &'static str)> {
    let trimmed = raw.trim();
    if let Some(start) = trimmed.find('"')
        && let Some(end_rel) = trimmed[start + 1..].find('"')
    {
        let end = start + 1 + end_rel;
        return Some((trimmed[start + 1..end].to_string(), "INCLUDE_QUOTE"));
    }
    if let Some(start) = trimmed.find('<')
        && let Some(end_rel) = trimmed[start + 1..].find('>')
    {
        let end = start + 1 + end_rel;
        return Some((trimmed[start + 1..end].to_string(), "INCLUDE_ANGLE"));
    }
    None
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|n| n.is_named())
        .collect()
}

fn named_child<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    named_children(node).into_iter().find(|n| n.kind() == kind)
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn is_branching_preproc(kind: &str) -> bool {
    kind.starts_with("preproc_if")
        || kind.starts_with("preproc_else")
        || kind.starts_with("preproc_elif")
}

fn specifier_has_body(node: Node<'_>) -> bool {
    node.child_by_field_name("body").is_some()
        || named_children(node)
            .into_iter()
            .any(|child| matches!(child.kind(), "field_declaration_list" | "enumerator_list"))
}

fn named_type_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    node.child_by_field_name("name")
        .or_else(|| {
            named_children(node)
                .into_iter()
                .find(|child| matches!(child.kind(), "type_identifier" | "identifier"))
        })
        .map(|n| text(n, src))
}

fn has_storage_class(node: Node<'_>, src: &SourceText<'_>, class: &str) -> bool {
    named_children(node)
        .into_iter()
        .any(|child| child.kind() == "storage_class_specifier" && text(child, src).trim() == class)
}

fn declaration_declarators(node: Node<'_>) -> Vec<Node<'_>> {
    named_children(node)
        .into_iter()
        .filter_map(|child| {
            if child.kind() == "init_declarator" {
                child
                    .child_by_field_name("declarator")
                    .or_else(|| declarator_child(child))
            } else if is_declarator_node(child) || is_name_node(child) {
                Some(child)
            } else {
                None
            }
        })
        .collect()
}

fn declarator_info(node: Node<'_>, src: &SourceText<'_>) -> Option<DeclInfo> {
    declarator_info_inner(node, src, None, false)
}

fn declarator_info_inner(
    node: Node<'_>,
    src: &SourceText<'_>,
    signature: Option<String>,
    is_function: bool,
) -> Option<DeclInfo> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" => Some(DeclInfo {
            short_name: text(node, src),
            signature,
            is_function,
        }),
        "function_declarator" => {
            let sig = Some(text(node, src));
            let inner = node
                .child_by_field_name("declarator")
                .or_else(|| declarator_child(node))?;
            let mut info = declarator_info_inner(inner, src, sig.clone(), true)?;
            if info.signature.is_none() {
                info.signature = sig;
            }
            info.is_function = true;
            Some(info)
        }
        "pointer_declarator"
        | "array_declarator"
        | "parenthesized_declarator"
        | "attributed_declarator"
        | "init_declarator" => {
            let inner = node
                .child_by_field_name("declarator")
                .or_else(|| declarator_child(node))?;
            declarator_info_inner(inner, src, signature, is_function)
        }
        _ => node
            .child_by_field_name("declarator")
            .or_else(|| declarator_child(node))
            .and_then(|inner| declarator_info_inner(inner, src, signature, is_function)),
    }
}

fn declarator_child(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node)
        .into_iter()
        .find(|child| is_declarator_node(*child) || is_name_node(*child))
}

fn is_declarator_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declarator"
            | "pointer_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator"
            | "init_declarator"
    )
}

fn is_name_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier" | "type_identifier" | "field_identifier"
    )
}

fn last_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut result = None;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "identifier" | "field_identifier" | "type_identifier"
        ) {
            result = Some(text(current, src));
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    result
}
