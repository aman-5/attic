//! C++ language specification (grammar: `tree-sitter-cpp` 0.23.x).
//!
//! The extractor records namespace/class structure, inheritance, includes,
//! `using` directives/declarations, templates, macros and intra-file calls.

use std::collections::HashSet;
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

pub(crate) static CPP_SPEC: CppSpec = CppSpec;

pub struct CppSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&CPP_SPEC)
}

impl TreeSitterLanguageSpec for CppSpec {
    fn analyzer_id(&self) -> &'static str {
        "cpp-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for C++: namespaces, classes, \
         inheritance, includes, using declarations, templates, methods and \
         intra-file name-based calls."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Cpp]
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
        tree_sitter_cpp::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "cpp"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st: St<'_, '_> = St {
            src,
            callables: Vec::new(),
            known_types: HashSet::new(),
            pending_calls: Vec::new(),
            saw_conditional: false,
        };
        walk_container(&mut st, root, &[], None, true, out);
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
    known_types: HashSet<String>,
    pending_calls: Vec<(Node<'tree>, usize)>,
    saw_conditional: bool,
}

#[derive(Clone)]
struct DeclInfo {
    parts: Vec<String>,
    short_name: String,
    signature: Option<String>,
    is_function: bool,
}

struct Classified {
    qualified_name: String,
    short_name: String,
    kind: SymbolKind,
    node_type: &'static str,
    is_method: bool,
}

#[derive(Clone)]
struct DeclContext {
    parent_idx: Option<usize>,
    top_level: bool,
    member_visibility: Option<String>,
    member_context: bool,
}

impl DeclContext {
    fn free(parent_idx: Option<usize>, top_level: bool) -> Self {
        Self {
            parent_idx,
            top_level,
            member_visibility: None,
            member_context: false,
        }
    }

    fn member(parent_idx: Option<usize>, visibility: &str) -> Self {
        Self {
            parent_idx,
            top_level: false,
            member_visibility: Some(visibility.to_string()),
            member_context: true,
        }
    }
}

fn walk_container<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'out>,
) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "translation_unit" | "declaration_list" => {
            for child in named_children(node) {
                walk_container(st, child, scope, parent_idx, top_level, out);
                if !out.tick() {
                    return;
                }
            }
        }
        "linkage_specification" => {
            for child in named_children(node) {
                walk_container(st, child, scope, parent_idx, top_level, out);
                if !out.tick() {
                    return;
                }
            }
        }
        "template_declaration" => {
            for child in named_children(node) {
                if child.kind() == "template_parameter_list" {
                    continue;
                }
                walk_container(st, child, scope, parent_idx, top_level, out);
                if !out.tick() {
                    return;
                }
            }
        }
        "namespace_definition" => extract_namespace(st, node, scope, parent_idx, top_level, out),
        "preproc_include" => extract_include(node, st.src, out),
        "preproc_def" | "preproc_function_def" => extract_macro(node, st.src, out),
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
            extract_classlike(st, node, scope, parent_idx, top_level, None, out)
        }
        "function_definition" => extract_function_definition(
            st,
            node,
            scope,
            DeclContext::free(parent_idx, top_level),
            out,
        ),
        "declaration" | "field_declaration" => extract_declaration_like(
            st,
            node,
            scope,
            DeclContext::free(parent_idx, top_level),
            out,
        ),
        "alias_declaration" => {
            extract_alias_declaration(node, st.src, scope, parent_idx, top_level, None, out)
        }
        "using_declaration" => extract_using(node, st.src, out),
        kind if is_branching_preproc(kind) => {
            mark_preprocessor_partial(st, out);
            for child in named_children(node) {
                walk_container(st, child, scope, parent_idx, top_level, out);
                if !out.tick() {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn extract_namespace<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'out>,
) {
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| named_child(node, "nested_namespace_specifier"))
        .or_else(|| named_child(node, "namespace_identifier"))
    else {
        return;
    };
    let parts = split_cpp_path(&text(name_node, st.src));
    if parts.is_empty() {
        return;
    }
    let mut qparts = scope.to_vec();
    qparts.extend(parts);
    let qualified = qparts.join(".");
    let short_name = qparts.last().cloned().unwrap_or_default();

    let Some(idx) = out.push_node(
        "NAMESPACE",
        &short_name,
        node,
        format!("cpp|{qualified}|NAMESPACE"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name,
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

    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| named_child(node, "declaration_list"))
    {
        walk_container(st, body, &qparts, Some(idx), false, out);
    }
}

fn extract_classlike<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    member_visibility: Option<String>,
    out: &mut Extraction<'out>,
) {
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| named_child(node, "type_identifier"))
        .or_else(|| named_child(node, "identifier"))
    else {
        return;
    };
    let name = text(name_node, st.src);
    let mut qparts = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");
    let node_type = match node.kind() {
        "class_specifier" => "CLASS",
        "struct_specifier" => "STRUCT",
        "union_specifier" => "UNION",
        "enum_specifier" => "ENUM",
        _ => return,
    };
    let Some(idx) = out.push_node(
        node_type,
        &name,
        node,
        format!("cpp|{qualified}|{node_type}"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    let has_body = specifier_has_body(node);
    let is_public = member_visibility.as_deref() == Some("public") || member_visibility.is_none();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: None,
        visibility: member_visibility.clone(),
        is_definition: has_body,
        node_index: Some(idx),
    });
    remember_type(&mut st.known_types, &qualified);
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(base_clause) = named_child(node, "base_class_clause") {
        for child in named_children(base_clause) {
            if child.kind() == "access_specifier" {
                continue;
            }
            let target = normalize_base_target(&text(child, st.src));
            if target.is_empty() {
                continue;
            }
            out.push_rel(
                "EXTENDS",
                target,
                child,
                ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| named_child(node, "field_declaration_list"))
    {
        walk_class_body(
            st,
            body,
            &qparts,
            Some(idx),
            default_member_access(node.kind()),
            out,
        );
    }
}

fn walk_class_body<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    body: Node<'tree>,
    scope: &[String],
    parent_idx: Option<usize>,
    default_access: &'static str,
    out: &mut Extraction<'out>,
) {
    let mut access = default_access.to_string();
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "access_specifier" => access = text(member, st.src),
            "template_declaration" => {
                for child in named_children(member) {
                    if child.kind() == "template_parameter_list" {
                        continue;
                    }
                    walk_class_member(st, child, scope, parent_idx, &access, out);
                }
            }
            _ => walk_class_member(st, member, scope, parent_idx, &access, out),
        }
    }
}

fn walk_class_member<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    parent_idx: Option<usize>,
    access: &str,
    out: &mut Extraction<'out>,
) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "function_definition" => extract_function_definition(
            st,
            node,
            scope,
            DeclContext::member(parent_idx, access),
            out,
        ),
        "declaration" | "field_declaration" => extract_declaration_like(
            st,
            node,
            scope,
            DeclContext::member(parent_idx, access),
            out,
        ),
        "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier" => {
            extract_classlike(
                st,
                node,
                scope,
                parent_idx,
                false,
                Some(access.to_string()),
                out,
            )
        }
        "alias_declaration" => extract_alias_declaration(
            node,
            st.src,
            scope,
            parent_idx,
            false,
            Some(access.to_string()),
            out,
        ),
        "using_declaration" => extract_using(node, st.src, out),
        kind if is_branching_preproc(kind) => {
            mark_preprocessor_partial(st, out);
            for child in named_children(node) {
                walk_class_member(st, child, scope, parent_idx, access, out);
            }
        }
        _ => {}
    }
}

fn extract_declaration_like<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    ctx: DeclContext,
    out: &mut Extraction<'out>,
) {
    let is_static = has_storage_class(node, st.src, "static");
    let is_extern = has_storage_class(node, st.src, "extern");
    let storage_visibility = if is_static {
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
            "class_specifier" | "struct_specifier" | "union_specifier" | "enum_specifier"
        ) && (specifier_has_body(child) || child.kind() == "enum_specifier")
        {
            extract_classlike(
                st,
                child,
                scope,
                ctx.parent_idx,
                ctx.top_level,
                ctx.member_visibility.clone(),
                out,
            );
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
            let classified = classify_decl(scope, &info.parts, &st.known_types, ctx.member_context);
            let Some(idx) = out.push_node(
                classified.node_type,
                &classified.short_name,
                decl,
                format!(
                    "cpp|{}|{}|{}",
                    classified.qualified_name,
                    classified.node_type,
                    info.signature.clone().unwrap_or_default()
                ),
                ctx.parent_idx,
            ) else {
                return;
            };
            let visibility = if classified.is_method {
                ctx.member_visibility.clone()
            } else {
                storage_visibility.clone()
            };
            let is_public = if classified.is_method {
                ctx.member_visibility.as_deref() == Some("public")
            } else {
                !is_static
            };
            out.push_symbol(CanonSymbol {
                qualified_name: classified.qualified_name.clone(),
                short_name: classified.short_name.clone(),
                kind: classified.kind,
                span: span_of(decl),
                is_public,
                disambiguator: None,
                signature: info.signature,
                visibility,
                is_definition: false,
                node_index: Some(idx),
            });
            remember_callable(&mut st.callables, classified.short_name);
            if ctx.top_level {
                out.mark_top_level(idx);
            }
        } else {
            let qualified_name = qualify(scope, &info.short_name);
            let is_public = if ctx.member_context {
                ctx.member_visibility.as_deref() == Some("public")
            } else {
                !is_static
            };
            let visibility = if ctx.member_context {
                ctx.member_visibility.clone()
            } else {
                storage_visibility.clone()
            };
            out.push_symbol(CanonSymbol {
                qualified_name,
                short_name: info.short_name,
                kind: SymbolKind::Variable,
                span: span_of(decl),
                is_public,
                disambiguator: None,
                signature: None,
                visibility,
                is_definition: ctx.member_context || !is_extern,
                node_index: None,
            });
        }
    }
}

fn extract_function_definition<'tree, 'src, 'out>(
    st: &mut St<'tree, 'src>,
    node: Node<'tree>,
    scope: &[String],
    ctx: DeclContext,
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
    let classified = classify_decl(scope, &info.parts, &st.known_types, ctx.member_context);
    let is_static = has_storage_class(node, st.src, "static");
    let visibility = if classified.is_method {
        ctx.member_visibility.clone()
    } else if is_static {
        Some("static".to_string())
    } else {
        None
    };
    let is_public = if classified.is_method {
        ctx.member_visibility.as_deref() == Some("public")
    } else {
        !is_static
    };

    let Some(idx) = out.push_node(
        classified.node_type,
        &classified.short_name,
        node,
        format!(
            "cpp|{}|{}|{}",
            classified.qualified_name,
            classified.node_type,
            info.signature.clone().unwrap_or_default()
        ),
        ctx.parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: classified.qualified_name.clone(),
        short_name: classified.short_name.clone(),
        kind: classified.kind,
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: info.signature,
        visibility,
        is_definition: true,
        node_index: Some(idx),
    });
    remember_callable(&mut st.callables, classified.short_name);
    if ctx.top_level {
        out.mark_top_level(idx);
    }
    if let Some(body) = node.child_by_field_name("body") {
        st.pending_calls.push((body, sym_idx));
    }
}

fn extract_alias_declaration(
    node: Node<'_>,
    src: &SourceText<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    member_visibility: Option<String>,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node
        .child_by_field_name("name")
        .or_else(|| named_child(node, "type_identifier"))
        .or_else(|| named_child(node, "identifier"))
    else {
        return;
    };
    let name = text(name_node, src);
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "TYPE_ALIAS",
        &name,
        node,
        format!("cpp|{qualified}|TYPE_ALIAS"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::TypeAlias,
        span: span_of(node),
        is_public: member_visibility.as_deref() == Some("public") || member_visibility.is_none(),
        disambiguator: None,
        signature: None,
        visibility: member_visibility,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_using(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let raw = text(node, src);
    let (kind, specifier) = if raw.contains("using namespace") {
        (
            "USING_NAMESPACE",
            named_children(node)
                .into_iter()
                .find(|child| {
                    matches!(
                        child.kind(),
                        "identifier" | "namespace_identifier" | "qualified_identifier"
                    )
                })
                .map(|child| text(child, src)),
        )
    } else {
        (
            "USING_DECLARATION",
            named_children(node)
                .into_iter()
                .find(|child| {
                    matches!(
                        child.kind(),
                        "qualified_identifier"
                            | "identifier"
                            | "namespace_identifier"
                            | "type_identifier"
                    )
                })
                .map(|child| text(child, src)),
        )
    };
    if let Some(specifier) = specifier {
        out.push_import(specifier, kind, node);
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
    let Some(idx) = out.push_node("MACRO", &name, node, format!("cpp|{name}|MACRO"), None) else {
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

fn classify_decl(
    scope: &[String],
    parts: &[String],
    known_types: &HashSet<String>,
    member_context: bool,
) -> Classified {
    let short_name = parts.last().cloned().unwrap_or_default();
    let scoped_parts = if scope.is_empty() {
        parts.to_vec()
    } else {
        let mut combined = scope.to_vec();
        combined.extend(parts.iter().cloned());
        combined
    };
    let explicit_owner = if parts.len() > 1 {
        parts[..parts.len() - 1].join(".")
    } else {
        String::new()
    };
    let scoped_owner = if parts.len() > 1 && !scope.is_empty() {
        let mut combined = scope.to_vec();
        combined.extend(parts[..parts.len() - 1].iter().cloned());
        combined.join(".")
    } else {
        String::new()
    };

    let qualified_parts = if member_context && parts.len() == 1 {
        let mut combined = scope.to_vec();
        combined.push(short_name.clone());
        combined
    } else if parts.len() > 1 && !scoped_owner.is_empty() && known_types.contains(&scoped_owner) {
        scoped_parts
    } else if parts.len() > 1 && known_types.contains(&explicit_owner) {
        parts.to_vec()
    } else if parts.len() == 1 && !scope.is_empty() {
        let mut combined = scope.to_vec();
        combined.push(short_name.clone());
        combined
    } else {
        parts.to_vec()
    };

    let owner_short = if qualified_parts.len() > 1 {
        qualified_parts[qualified_parts.len() - 2].clone()
    } else {
        String::new()
    };
    let owner_full = if qualified_parts.len() > 1 {
        qualified_parts[..qualified_parts.len() - 1].join(".")
    } else {
        String::new()
    };
    let is_method = member_context
        || (!owner_full.is_empty() && known_types.contains(&owner_full))
        || (!owner_short.is_empty() && known_types.contains(&owner_short));
    let node_type = if is_method && short_name == owner_short {
        "CONSTRUCTOR"
    } else if is_method && short_name == format!("~{owner_short}") {
        "DESTRUCTOR"
    } else if is_method {
        "METHOD"
    } else {
        "FUNCTION"
    };

    Classified {
        qualified_name: qualified_parts.join("."),
        short_name,
        kind: if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        },
        node_type,
        is_method,
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
    match node.kind() {
        "call_expression" => {
            if let Some(func) = node.child_by_field_name("function")
                && let Some(callee) = last_name(func, src)
                && callables.iter().any(|known| known == &callee)
            {
                out.push_rel(
                    "CALL",
                    callee,
                    node,
                    ResolutionLevel::SymbolResolved,
                    0.8,
                    Some(owner_sym),
                );
                return;
            }
            recurse_calls(node, src, callables, owner_sym, out);
        }
        "new_expression" => {
            if let Some(ctor) = node.child_by_field_name("type")
                && let Some(callee) = last_name(ctor, src)
                && callables.iter().any(|known| known == &callee)
            {
                out.push_rel(
                    "CALL",
                    callee,
                    node,
                    ResolutionLevel::SymbolResolved,
                    0.75,
                    Some(owner_sym),
                );
                return;
            }
            recurse_calls(node, src, callables, owner_sym, out);
        }
        _ => recurse_calls(node, src, callables, owner_sym, out),
    }
}

fn recurse_calls(
    node: Node<'_>,
    src: &SourceText<'_>,
    callables: &[String],
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
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

fn remember_type(known_types: &mut HashSet<String>, qualified: &str) {
    let _ = known_types.insert(qualified.to_string());
    if let Some(short) = qualified.rsplit('.').next() {
        let _ = known_types.insert(short.to_string());
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

fn qualify(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        let mut qparts = scope.to_vec();
        qparts.push(name.to_string());
        qparts.join(".")
    }
}

fn split_cpp_path(raw: &str) -> Vec<String> {
    let compact: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let mut parts = Vec::new();
    let mut segment = String::new();
    let mut depth = 0u32;
    let chars: Vec<char> = compact.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '<' && !segment.starts_with("operator") {
            depth = depth.saturating_add(1);
            i += 1;
            continue;
        }
        if ch == '>' && depth > 0 {
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 && ch == ':' && i + 1 < chars.len() && chars[i + 1] == ':' {
            if !segment.is_empty() {
                parts.push(segment.clone());
                segment.clear();
            }
            i += 2;
            continue;
        }
        if depth == 0 {
            segment.push(ch);
        }
        i += 1;
    }
    if !segment.is_empty() {
        parts.push(segment);
    }
    parts
}

fn normalize_base_target(raw: &str) -> String {
    let last = raw
        .split_whitespace()
        .last()
        .unwrap_or(raw)
        .trim_matches(':')
        .trim();
    split_cpp_path(last).join(".")
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
        || named_children(node).into_iter().any(|child| {
            matches!(
                child.kind(),
                "field_declaration_list" | "enumerator_list" | "declaration_list"
            )
        })
}

fn default_member_access(kind: &str) -> &'static str {
    match kind {
        "class_specifier" => "private",
        _ => "public",
    }
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
        "identifier" | "type_identifier" | "field_identifier" | "namespace_identifier" => {
            let short_name = text(node, src);
            Some(DeclInfo {
                parts: vec![short_name.clone()],
                short_name,
                signature,
                is_function,
            })
        }
        "operator_name" | "destructor_name" | "qualified_identifier" | "scoped_identifier" => {
            let parts = split_cpp_path(&text(node, src));
            let short_name = parts.last()?.clone();
            Some(DeclInfo {
                parts,
                short_name,
                signature,
                is_function,
            })
        }
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
        | "reference_declarator"
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
            | "reference_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator"
            | "init_declarator"
    )
}

fn is_name_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "field_identifier"
            | "namespace_identifier"
            | "operator_name"
            | "destructor_name"
            | "qualified_identifier"
            | "scoped_identifier"
    )
}

fn last_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut result = None;
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        if matches!(
            current.kind(),
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "namespace_identifier"
                | "operator_name"
                | "destructor_name"
        ) {
            result = Some(text(current, src));
        } else if matches!(current.kind(), "qualified_identifier" | "scoped_identifier") {
            let parts = split_cpp_path(&text(current, src));
            if let Some(last) = parts.last() {
                result = Some(last.clone());
            }
        }
        let mut cursor = current.walk();
        for child in current.children(&mut cursor) {
            stack.push(child);
        }
    }
    result
}
