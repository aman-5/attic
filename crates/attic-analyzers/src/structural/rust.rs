//! Rust language specification (grammar: `tree-sitter-rust` 0.24.x).
//!
//! The pinned grammar exposes `use` declarations, `mod` items, structs/enums/
//! unions/traits/impls/functions/consts/statics/type aliases, declarative
//! macros and call expressions. This adapter records syntax-backed structure,
//! import facts, trait/impl relationships and partial intra-file calls without
//! expanding macros or claiming build-system resolution.

use std::collections::HashMap;
use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static RUST_SPEC: RustSpec = RustSpec;

pub struct RustSpec;

pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&RUST_SPEC)
}

impl TreeSitterLanguageSpec for RustSpec {
    fn analyzer_id(&self) -> &'static str {
        "rust-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Rust: modules, types, traits, \
         impl methods, imports, supertraits and partial intra-file call edges. \
         Macro invocations are not expanded."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Rust]
    }

    fn capabilities(&self) -> AnalyzerCapabilities {
        AnalyzerCapabilities {
            entries: vec![
                (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                (CapabilityKind::SymbolExtraction, CapabilityLevel::Full),
                (CapabilityKind::ImportExtraction, CapabilityLevel::Full),
                // Calls and trait relationships are syntax-backed only; macro
                // expansion and name resolution remain intentionally partial.
                (CapabilityKind::ReferenceExtraction, CapabilityLevel::Basic),
                (
                    CapabilityKind::RelationshipResolution,
                    CapabilityLevel::Basic,
                ),
            ],
        }
    }

    fn grammar(&self) -> tree_sitter_language::LanguageFn {
        tree_sitter_rust::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "rust"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
            owner_symbols_by_qualified: HashMap::new(),
            owner_symbols_by_short: HashMap::new(),
            owner_nodes_by_qualified: HashMap::new(),
            owner_nodes_by_short: HashMap::new(),
            soft_item_limit: (src.len() >= 4 * 1024 * 1024).then_some(1024),
            top_level_items_seen: 0,
        };
        let scope: Vec<String> = Vec::new();
        extract_items(&mut st, root, &scope, None, true, out);
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
    owner_symbols_by_qualified: HashMap<String, usize>,
    owner_symbols_by_short: HashMap<String, usize>,
    owner_nodes_by_qualified: HashMap<String, usize>,
    owner_nodes_by_short: HashMap<String, usize>,
    soft_item_limit: Option<usize>,
    top_level_items_seen: usize,
}

fn extract_items(
    st: &mut St<'_>,
    container: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    for child in named_children(container) {
        if top_level
            && st
                .soft_item_limit
                .is_some_and(|limit| st.top_level_items_seen >= limit)
        {
            break;
        }
        if top_level {
            st.top_level_items_seen += 1;
        }
        if !out.tick() {
            return;
        }
        match child.kind() {
            "use_declaration" => extract_use(child, st.src, out),
            "extern_crate_declaration" => extract_extern_crate(child, st.src, out),
            "mod_item" => extract_module(st, child, scope, parent_idx, top_level, out),
            "struct_item" => extract_named_type(
                st,
                child,
                scope,
                parent_idx,
                top_level,
                "STRUCT",
                SymbolKind::Class,
                out,
            ),
            "enum_item" => extract_named_type(
                st,
                child,
                scope,
                parent_idx,
                top_level,
                "ENUM",
                SymbolKind::Class,
                out,
            ),
            "union_item" => extract_named_type(
                st,
                child,
                scope,
                parent_idx,
                top_level,
                "UNION",
                SymbolKind::Class,
                out,
            ),
            "trait_item" => extract_trait(st, child, scope, parent_idx, top_level, out),
            "impl_item" => extract_impl(st, child, scope, out),
            "function_item" => extract_function(st, child, scope, parent_idx, top_level, out),
            "const_item" => extract_value_item(
                st,
                child,
                scope,
                parent_idx,
                top_level,
                "CONST",
                SymbolKind::Constant,
                out,
            ),
            "static_item" => extract_value_item(
                st,
                child,
                scope,
                parent_idx,
                top_level,
                "STATIC",
                SymbolKind::Variable,
                out,
            ),
            "type_item" => extract_type_alias(st, child, scope, parent_idx, top_level, out),
            "macro_definition" => extract_macro(st, child, scope, parent_idx, top_level, out),
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
    let name = text(name_node, st.src);
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "MODULE",
        &name,
        node,
        format!("rust|{qualified}|MODULE"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Module,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    remember_owner(st, &qualified, &name, sym_idx, idx);

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        let mut next_scope = scope.to_vec();
        next_scope.push(name);
        extract_items(st, body, &next_scope, Some(idx), false, out);
    } else {
        out.push_import(name, "MOD", node);
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_named_type(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    node_tag: &str,
    kind: SymbolKind,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        node_tag,
        &name,
        node,
        format!("rust|{qualified}|{node_tag}"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    remember_owner(st, &qualified, &name, sym_idx, idx);

    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_trait(
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
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "INTERFACE",
        &name,
        node,
        format!("rust|{qualified}|INTERFACE"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::Interface,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    remember_owner(st, &qualified, &name, sym_idx, idx);

    if let Some(bounds) = node.child_by_field_name("bounds") {
        for target in named_children(bounds) {
            if target.kind() == "lifetime" {
                continue;
            }
            if let Some(name) = base_name(&text(target, st.src)) {
                out.push_rel(
                    "EXTENDS",
                    name,
                    target,
                    ResolutionLevel::Syntactic,
                    0.5,
                    Some(sym_idx),
                );
            }
        }
    }

    if top_level {
        out.mark_top_level(idx);
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
            "function_item" => {
                extract_method_like(st, member, &qualified, Some(idx), true, true, out)
            }
            "function_signature_item" => {
                extract_trait_signature(st, member, &qualified, Some(idx), out)
            }
            "type_item" => extract_associated_type(st, member, &qualified, Some(idx), true, out),
            "associated_type" => {
                extract_associated_type(st, member, &qualified, Some(idx), false, out)
            }
            "const_item" => extract_associated_value(
                st,
                member,
                &qualified,
                Some(idx),
                SymbolKind::Constant,
                true,
                out,
            ),
            _ => extract_items(st, member, &next_scope, Some(idx), false, out),
        }
    }
}

fn extract_impl(st: &mut St<'_>, node: Node<'_>, scope: &[String], out: &mut Extraction<'_>) {
    let Some(type_node) = node.child_by_field_name("type") else {
        return;
    };
    let owner_short = base_name(&text(type_node, st.src)).unwrap_or_default();
    if owner_short.is_empty() {
        return;
    }
    let owner_qualified = qualify(scope, &owner_short);
    let owner_symbol = lookup_owner_symbol(st, scope, &owner_short);
    let owner_node = lookup_owner_node(st, scope, &owner_short);

    if let Some(trait_node) = node.child_by_field_name("trait")
        && let Some(target) = base_name(&text(trait_node, st.src))
    {
        out.push_rel(
            "IMPLEMENTS",
            target,
            trait_node,
            ResolutionLevel::Syntactic,
            0.5,
            owner_symbol,
        );
    }

    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "function_item" => {
                extract_method_like(st, member, &owner_qualified, owner_node, false, true, out)
            }
            "type_item" => {
                extract_associated_type(st, member, &owner_qualified, owner_node, true, out)
            }
            "const_item" => extract_associated_value(
                st,
                member,
                &owner_qualified,
                owner_node,
                SymbolKind::Constant,
                true,
                out,
            ),
            "static_item" => extract_associated_value(
                st,
                member,
                &owner_qualified,
                owner_node,
                SymbolKind::Variable,
                true,
                out,
            ),
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
    let signature = function_signature(node, &name, st.src);
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "FUNCTION",
        &name,
        node,
        format!("rust|{qualified}|FUNCTION|{signature}"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: Some(signature),
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name);

    if top_level {
        out.mark_top_level(idx);
    }
    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, Some(sym_idx), out);
    }
}

fn extract_method_like(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    default_public: bool,
    is_definition: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let signature = function_signature(node, &name, st.src);
    let qualified = format!("{owner_qualified}.{name}");
    let Some(idx) = out.push_node(
        "METHOD",
        &name,
        node,
        format!("rust|{qualified}|METHOD|{signature}"),
        owner_node,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: is_public_item(node, st.src)
            || (default_public && visibility_of(node, st.src).is_none()),
        disambiguator: None,
        signature: Some(signature),
        visibility: visibility_of(node, st.src),
        is_definition,
        node_index: Some(idx),
    });
    st.locals.push(name);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, Some(sym_idx), out);
    }
}

fn extract_trait_signature(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let signature = function_signature(node, &name, st.src);
    out.push_symbol(CanonSymbol {
        qualified_name: format!("{owner_qualified}.{name}"),
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(signature),
        visibility: visibility_of(node, st.src),
        is_definition: false,
        node_index: owner_node,
    });
    st.locals.push(name);
}

#[allow(clippy::too_many_arguments)]
fn extract_value_item(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    node_tag: &str,
    kind: SymbolKind,
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
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        node_tag,
        &name,
        node,
        format!("rust|{qualified}|{node_tag}|{ty}"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: (!ty.is_empty()).then_some(ty),
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_associated_value(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    kind: SymbolKind,
    is_definition: bool,
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
    out.push_symbol(CanonSymbol {
        qualified_name: format!("{owner_qualified}.{name}"),
        short_name: name,
        kind,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: (!ty.is_empty()).then_some(ty),
        visibility: visibility_of(node, st.src),
        is_definition,
        node_index: owner_node,
    });
}

fn extract_type_alias(
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
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "TYPE_ALIAS",
        &name,
        node,
        format!("rust|{qualified}|TYPE_ALIAS"),
        parent_idx,
    ) else {
        return;
    };
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: SymbolKind::TypeAlias,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: node.child_by_field_name("type").map(|n| text(n, st.src)),
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    remember_owner(st, &qualified, &name, sym_idx, idx);
    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_associated_type(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_node: Option<usize>,
    is_definition: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let signature = node
        .child_by_field_name("type")
        .map(|n| text(n, st.src))
        .or_else(|| node.child_by_field_name("bounds").map(|n| text(n, st.src)));
    out.push_symbol(CanonSymbol {
        qualified_name: format!("{owner_qualified}.{name}"),
        short_name: name,
        kind: SymbolKind::TypeAlias,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature,
        visibility: visibility_of(node, st.src),
        is_definition,
        node_index: owner_node,
    });
}

fn extract_macro(
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
    let qualified = qualify(scope, &name);
    let Some(idx) = out.push_node(
        "MACRO",
        &name,
        node,
        format!("rust|{qualified}|MACRO"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::Macro,
        span: span_of(node),
        is_public: is_public_item(node, st.src),
        disambiguator: None,
        signature: None,
        visibility: visibility_of(node, st.src),
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }
}

fn extract_use(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let Some(argument) = node.child_by_field_name("argument") else {
        return;
    };
    let mut flattened = Vec::new();
    flatten_use(argument, None, src, &mut flattened);
    for raw in flattened {
        if !raw.is_empty() {
            out.push_import(raw, "IMPORT", node);
        }
    }
}

fn extract_extern_crate(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, src);
    if !name.is_empty() {
        out.push_import(name, "EXTERN_CRATE", node);
    }
}

fn flatten_use(node: Node<'_>, prefix: Option<&str>, src: &SourceText<'_>, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "scoped_identifier" | "crate" | "self" | "super" => {
            if let Some(joined) = join_use(prefix, &text(node, src)) {
                out.push(joined);
            }
        }
        "use_wildcard" => {
            if let Some(prefix) = prefix {
                out.push(format!("{prefix}::*"));
            }
        }
        "use_as_clause" => {
            if let Some(path) = node.child_by_field_name("path") {
                let raw = text(path, src);
                if raw == "self" {
                    if let Some(prefix) = prefix {
                        out.push(prefix.to_string());
                    }
                } else if let Some(joined) = join_use(prefix, &raw) {
                    out.push(joined);
                }
            }
        }
        "scoped_use_list" => {
            let next_prefix = node
                .child_by_field_name("path")
                .map(|path| {
                    let raw = text(path, src);
                    join_use(prefix, &raw).unwrap_or(raw)
                })
                .or_else(|| prefix.map(str::to_string));
            if let Some(list) = node.child_by_field_name("list") {
                flatten_use(list, next_prefix.as_deref(), src, out);
            }
        }
        "use_list" => {
            for child in named_children(node) {
                flatten_use(child, prefix, src, out);
            }
        }
        _ => {}
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
    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && let Some(callee) = call_target_name(func, st.src)
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

    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for child in kids {
        collect_calls(st, child, owner_sym, out);
    }
}

fn remember_owner(
    st: &mut St<'_>,
    qualified: &str,
    short: &str,
    symbol_idx: usize,
    node_idx: usize,
) {
    st.owner_symbols_by_qualified
        .insert(qualified.to_string(), symbol_idx);
    st.owner_nodes_by_qualified
        .insert(qualified.to_string(), node_idx);
    st.owner_symbols_by_short
        .insert(short.to_string(), symbol_idx);
    st.owner_nodes_by_short.insert(short.to_string(), node_idx);
}

fn lookup_owner_symbol(st: &St<'_>, scope: &[String], short: &str) -> Option<usize> {
    let qualified = qualify(scope, short);
    st.owner_symbols_by_qualified
        .get(&qualified)
        .copied()
        .or_else(|| st.owner_symbols_by_short.get(short).copied())
}

fn lookup_owner_node(st: &St<'_>, scope: &[String], short: &str) -> Option<usize> {
    let qualified = qualify(scope, short);
    st.owner_nodes_by_qualified
        .get(&qualified)
        .copied()
        .or_else(|| st.owner_nodes_by_short.get(short).copied())
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn qualify(scope: &[String], name: &str) -> String {
    if scope.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", scope.join("."), name)
    }
}

fn visibility_of(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    named_children(node)
        .into_iter()
        .find(|child| child.kind() == "visibility_modifier")
        .map(|child| text(child, src))
}

fn is_public_item(node: Node<'_>, src: &SourceText<'_>) -> bool {
    visibility_of(node, src)
        .map(|v| v.starts_with("pub"))
        .unwrap_or(false)
}

fn function_signature(node: Node<'_>, name: &str, src: &SourceText<'_>) -> String {
    let params = node
        .child_by_field_name("parameters")
        .map(|n| text(n, src))
        .unwrap_or_default();
    let ret = node
        .child_by_field_name("return_type")
        .map(|n| format!(" -> {}", text(n, src)))
        .unwrap_or_default();
    format!("{name}{params}{ret}")
}

fn join_use(prefix: Option<&str>, segment: &str) -> Option<String> {
    let segment = segment.trim();
    if segment.is_empty() {
        return None;
    }
    if segment == "self" {
        return prefix.map(str::to_string);
    }
    Some(match prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}::{segment}"),
        _ => segment.to_string(),
    })
}

fn base_name(raw: &str) -> Option<String> {
    let normalized = raw.replace("::", ".");
    let segment = normalized
        .rsplit('.')
        .find(|part| !part.trim().is_empty())?
        .trim();
    let truncated = segment
        .split(['<', '[', '(', ' ', '&'])
        .next()
        .unwrap_or(segment)
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
    (!truncated.is_empty()).then(|| truncated.to_string())
}

fn call_target_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    match node.kind() {
        "generic_function" => node
            .child_by_field_name("function")
            .and_then(|inner| call_target_name(inner, src)),
        _ => base_name(&text(node, src)),
    }
}
