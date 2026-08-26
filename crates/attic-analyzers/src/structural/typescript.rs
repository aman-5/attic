//! TypeScript language specification (grammar: `tree-sitter-typescript`
//! 0.23.x, `LANGUAGE_TYPESCRIPT`). Reuses the shared ECMAScript walkers from
//! [`super::javascript`] and adds the TS-only declaration kinds observed in
//! probe output: `interface_declaration`, `type_alias_declaration`,
//! `enum_declaration`, `abstract_class_declaration`,
//! `internal_module` (namespace), `abstract_method_signature`,
//! `method_signature`, `property_signature`, `function_signature`,
//! `ambient_declaration`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel};
use crate::structural::javascript as js;
use crate::structural::{
    CanonSymbol, Extraction, LanguageSpec, SourceText, make_analyzer, span_of,
};

pub(crate) static TYPESCRIPT_SPEC: TypeScriptSpec = TypeScriptSpec;

pub struct TypeScriptSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&TYPESCRIPT_SPEC)
}

impl LanguageSpec for TypeScriptSpec {
    fn analyzer_id(&self) -> &'static str {
        "typescript-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for TypeScript: everything the \
         JavaScript analyzer provides plus interfaces, type aliases, enums, \
         namespaces, abstract classes/signatures and type-only imports."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::TypeScript]
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
        // The TypeScript grammar crate exposes two languages; the plain TS
        // variant covers .ts/.tsx sources.
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    }

    fn language_tag(&self) -> &'static str {
        "typescript"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = js::JsState {
            lang_tag: "typescript",
            src,
            locals: Vec::new(),
            imported: Vec::new(),
        };
        walk_ts_container(&mut st, root, &[], None, true, out);
    }
}

fn walk_ts_container(
    st: &mut js::JsState<'_>,
    container: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    for child in js::named_children(container) {
        if !out.tick() {
            return;
        }
        match child.kind() {
            "interface_declaration" => {
                extract_interface(st, child, scope, parent_idx, top_level, out)
            }
            "type_alias_declaration" => {
                extract_type_alias(st, child, scope, parent_idx, top_level, out)
            }
            "enum_declaration" => extract_enum(st, child, scope, parent_idx, top_level, out),
            "internal_module" => extract_namespace(st, child, scope, parent_idx, top_level, out),
            "abstract_class_declaration" => {
                extract_abstract_class(st, child, scope, parent_idx, top_level, out)
            }
            "function_signature" => extract_function_signature(st, child, scope, out),
            "declare_statement" | "ambient_declaration" => {
                walk_ts_container(st, child, scope, parent_idx, false, out)
            }
            "export_statement" => {
                // Exported TS-only declarations need unwrapping here; JS kinds
                // delegate to the shared walker.
                let exported_js_kind = js::named_children(child).into_iter().any(|d| {
                    matches!(
                        d.kind(),
                        "class_declaration"
                            | "function_declaration"
                            | "generator_function_declaration"
                            | "lexical_declaration"
                            | "variable_declaration"
                    )
                });
                if exported_js_kind {
                    js::walk_container(st, child, scope, parent_idx, top_level, out);
                } else {
                    for d in js::named_children(child) {
                        match d.kind() {
                            "interface_declaration" => {
                                extract_interface(st, d, scope, parent_idx, top_level, out)
                            }
                            "type_alias_declaration" => {
                                extract_type_alias(st, d, scope, parent_idx, top_level, out)
                            }
                            "enum_declaration" => {
                                extract_enum(st, d, scope, parent_idx, top_level, out)
                            }
                            "abstract_class_declaration" => {
                                extract_abstract_class(st, d, scope, parent_idx, top_level, out)
                            }
                            "internal_module" => {
                                extract_namespace(st, d, scope, parent_idx, top_level, out)
                            }
                            _ => {}
                        }
                    }
                    // Also record re-export specifiers (`export * from "./x"`).
                    let has_from = child
                        .children(&mut child.walk())
                        .any(|c| !c.is_named() && c.kind() == "from");
                    if has_from && let Some(spec) = js::last_string_fragment(child, st.src) {
                        out.push_import(spec, "EXPORT_FROM", child);
                    }
                }
            }
            _ => js::walk_stmt(st, child, scope, parent_idx, top_level, out),
        }
    }
}

// ---------------------------------------------------------------------------
// Interfaces / type aliases / enums / namespaces
// ---------------------------------------------------------------------------

fn qname_of(scope: &[String], name: &str) -> String {
    let mut parts = scope.to_vec();
    parts.push(name.to_string());
    parts.join(".")
}

fn extract_interface(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let qualified = qname_of(scope, &name);
    let identity = format!("{}|{qualified}|INTERFACE", st.lang_tag);
    let idx = out.push_node("INTERFACE", &name, node, identity, parent_idx);

    let exported = node
        .parent()
        .is_some_and(|p| p.kind() == "export_statement");
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name,
        kind: SymbolKind::Interface,
        span: span_of(node),
        is_public: true || exported,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    // Extends clause on interfaces (extends_type_clause / extends_clause).
    for ext in js::named_children(node)
        .into_iter()
        .filter(|c| c.kind() == "extends_type_clause" || c.kind() == "extends_clause")
    {
        for t in js::implements_targets(ext) {
            out.push_rel(
                "EXTENDS",
                js::text(t, st.src),
                ext,
                crate::api::ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    // Members are API surface: signatures only (is_definition=false).
    if let Some(body) = named_child(node, "interface_body") {
        for member in js::named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "property_signature" => member_signature(
                    st,
                    member,
                    "Variable",
                    SymbolKind::Variable,
                    &qualified,
                    idx,
                    out,
                ),
                "method_signature" => member_signature(
                    st,
                    member,
                    "Method",
                    SymbolKind::Method,
                    &qualified,
                    idx,
                    out,
                ),
                _ => {}
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn member_signature(
    st: &mut js::JsState<'_>,
    member: Node<'_>,
    node_tag: &str,
    kind: SymbolKind,
    owner_qname: &str,
    owner_idx: usize,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = member.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let params = member.child_by_field_name("parameters");
    let params_text = params.map(|p| js::text(p, st.src)).unwrap_or_default();
    out.push_symbol(CanonSymbol {
        qualified_name: format!("{owner_qname}.{name}"),
        short_name: name,
        kind,
        span: span_of(member),
        is_public: true,
        disambiguator: None,
        signature: (!params_text.is_empty()).then(|| params_text.to_string()),
        visibility: None,
        is_definition: false,
        node_index: Some(owner_idx),
    });
    let _ = node_tag;
}

fn extract_type_alias(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let qualified = qname_of(scope, &name);
    let identity = format!("{}|{qualified}|TYPE_ALIAS", st.lang_tag);
    let idx = out.push_node("TYPE_ALIAS", &name, node, identity, parent_idx);
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::TypeAlias,
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

fn extract_enum(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let qualified = qname_of(scope, &name);
    let identity = format!("{}|{qualified}|ENUM", st.lang_tag);
    let idx = out.push_node("ENUM", &name, node, identity, parent_idx);

    let sym_idx_holder = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name,
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    let _ = sym_idx_holder;
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = named_child(node, "enum_body") {
        for member in js::named_children(body) {
            if !out.tick() {
                return;
            }
            let member_name = match member.kind() {
                "enum_assignment" => member.child_by_field_name("name"),
                "property_identifier" => Some(member),
                _ => None,
            };
            if let Some(mn) = member_name {
                let m_name = js::text(mn, st.src);
                out.push_symbol(CanonSymbol {
                    qualified_name: format!("{qualified}.{m_name}"),
                    short_name: m_name,
                    kind: SymbolKind::Constant,
                    span: span_of(member),
                    is_public: true,
                    disambiguator: None,
                    signature: None,
                    visibility: None,
                    is_definition: true,
                    node_index: Some(idx),
                });
            }
        }
    }
}

fn extract_namespace(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) =
        named_child(node, "identifier").or_else(|| named_child(node, "type_identifier"))
    else {
        return;
    };
    let name = js::text(name_node, st.src);
    let qualified = qname_of(scope, &name);
    let identity = format!("{}|{qualified}|NAMESPACE", st.lang_tag);
    let idx = out.push_node("NAMESPACE", &name, node, identity, parent_idx);
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name,
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
    if let Some(body) = node.child_by_field_name("body") {
        walk_ts_container(st, body, &[qualified], Some(idx), false, out);
    }
}

// ---------------------------------------------------------------------------
// Abstract classes and signatures
// ---------------------------------------------------------------------------

fn extract_abstract_class(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    // Shared class walker handles heritage/members/extends/implements;
    // abstract-ness is preserved by the ABSTRACT_CLASS node tag below via a
    // small wrapper: we re-tag after extraction is not possible through the
    // shared fn, so replicate its core here with the different tag.
    let exported = node
        .parent()
        .is_some_and(|p| p.kind() == "export_statement");
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let mut qparts: Vec<String> = scope.to_vec();
    qparts.push(name.clone());
    let qualified = qparts.join(".");

    let identity = format!("{}|{qualified}|ABSTRACT_CLASS", st.lang_tag);
    let idx = out.push_node("ABSTRACT_CLASS", &name, node, identity, parent_idx);
    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name,
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: exported || true,
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });

    // Heritage (extends + implements), mirroring shared logic.
    let heritage = node.child_by_field_name("heritage").or_else(|| {
        js::named_children(node)
            .into_iter()
            .find(|c| c.kind() == "class_heritage")
    });
    if let Some(h) = heritage
        && let Some(base) = js::first_identifier_text(h, st.src)
    {
        out.push_rel(
            "EXTENDS",
            base,
            h,
            crate::api::ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }
    if let Some(clause) = js::find_descendant(node, "implements_clause") {
        for t in js::implements_targets(clause) {
            out.push_rel(
                "IMPLEMENTS",
                js::text(t, st.src),
                clause,
                crate::api::ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        for member in js::named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "abstract_method_signature" => {
                    let Some(mn) = member.child_by_field_name("name") else {
                        continue;
                    };
                    let m_name = js::text(mn, st.src);
                    let params = member.child_by_field_name("parameters");
                    let params_text = params.map(|p| js::text(p, st.src)).unwrap_or_default();
                    let m_qualified = format!("{qualified}.{m_name}");
                    let m_idx = out.push_node(
                        "ABSTRACT_METHOD",
                        &m_name,
                        member,
                        format!("{}|{m_qualified}|ABSTRACT_METHOD", st.lang_tag),
                        Some(idx),
                    );
                    out.push_symbol(CanonSymbol {
                        qualified_name: m_qualified,
                        short_name: m_name.clone(),
                        kind: SymbolKind::Method,
                        span: span_of(member),
                        is_public: true,
                        disambiguator: None,
                        signature: Some(format!("{m_name}{params_text}")),
                        visibility: None,
                        is_definition: false,
                        node_index: Some(m_idx),
                    });
                }
                "method_definition" => {
                    js::extract_method_definition(st, member, &qparts, Some(idx), sym_idx, out)
                }
                "field_definition" | "public_field_definition" => {
                    js::extract_field_definition(st, member, &qparts, out)
                }
                _ => {}
            }
        }
    }
}

fn extract_function_signature(
    st: &mut js::JsState<'_>,
    node: Node<'_>,
    scope: &[String],
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = js::text(name_node, st.src);
    let qualified = qname_of(scope, &name);
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name,
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(js::text(node, st.src)),
        visibility: None,
        is_definition: false,
        node_index: None,
    });
}

// ---------------------------------------------------------------------------

/// First direct named child with `kind`.
fn named_child<'t>(node: Node<'t>, kind: &'static str) -> Option<Node<'t>> {
    js::named_children(node)
        .into_iter()
        .find(|c| c.kind() == kind)
}
