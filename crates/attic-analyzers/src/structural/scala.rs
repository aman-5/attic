//! Scala language specification (grammar: `tree-sitter-scala` 0.26.x).
//!
//! Grounded in parse trees produced by the pinned grammar. Key observed kinds:
//! `compilation_unit`, `package_clause`, `import_declaration`,
//! `class_definition`, `trait_definition`, `object_definition`,
//! `enum_definition`, `template_body`, `extends_clause`,
//! `function_definition`, `val_definition`, `var_definition`,
//! `type_definition`, `call_expression`, `identifier`, `type_identifier`,
//! `generic_type`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static SCALA_SPEC: ScalaSpec = ScalaSpec;

pub struct ScalaSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&SCALA_SPEC)
}

impl TreeSitterLanguageSpec for ScalaSpec {
    fn analyzer_id(&self) -> &'static str {
        "scala-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Scala: structure, symbols \
         (classes/traits/objects, defs, vals/vars, type aliases), imports, \
         heritage via extends/with clauses, and intra-file call edges."
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
        tree_sitter_scala::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "scala"
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
                "package_clause" => st.package = parse_package(child, st.src),
                "import_declaration" => extract_import(child, st.src, out),
                _ => {}
            }
        }

        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "class_definition" => {
                    extract_type_decl(&mut st, child, &[], None, true, OwnerKind::Class, out)
                }
                "trait_definition" => {
                    extract_type_decl(&mut st, child, &[], None, true, OwnerKind::Interface, out)
                }
                "object_definition" => {
                    extract_type_decl(&mut st, child, &[], None, true, OwnerKind::Module, out)
                }
                "enum_definition" => {
                    extract_type_decl(&mut st, child, &[], None, true, OwnerKind::Class, out)
                }
                "function_definition" => {
                    extract_function(&mut st, child, &[], None, true, false, out)
                }
                "val_definition" | "var_definition" => extract_value_def(&mut st, child, &[], out),
                "type_definition" => extract_type_alias(&mut st, child, &[], None, true, out),
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

#[derive(Clone, Copy)]
enum OwnerKind {
    Class,
    Interface,
    Module,
}

fn extract_type_decl(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    owner_kind: OwnerKind,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let raw = text(node, st.src);
    let qualified = qname(st, scope, &name);
    let (node_tag, sym_kind) = match owner_kind {
        OwnerKind::Class => ("CLASS", SymbolKind::Class),
        OwnerKind::Interface => ("INTERFACE", SymbolKind::Interface),
        OwnerKind::Module => ("OBJECT", SymbolKind::Module),
    };

    let Some(idx) = out.push_node(
        node_tag,
        &name,
        node,
        format!("scala|{qualified}|{node_tag}"),
        parent_idx,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: sym_kind,
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

    extract_heritage(st, node, owner_kind, sym_idx, out);

    let mut next_scope = scope.to_vec();
    next_scope.push(name);
    if let Some(body) = find_child_kind(node, "template_body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "function_definition" => {
                    extract_function(st, member, &next_scope, Some(idx), false, true, out)
                }
                "val_definition" | "var_definition" => {
                    extract_value_def(st, member, &next_scope, out)
                }
                "type_definition" => {
                    extract_type_alias(st, member, &next_scope, Some(idx), false, out)
                }
                "class_definition" => extract_type_decl(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    OwnerKind::Class,
                    out,
                ),
                "trait_definition" => extract_type_decl(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    OwnerKind::Interface,
                    out,
                ),
                "object_definition" => extract_type_decl(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    OwnerKind::Module,
                    out,
                ),
                "enum_definition" => extract_type_decl(
                    st,
                    member,
                    &next_scope,
                    Some(idx),
                    false,
                    OwnerKind::Class,
                    out,
                ),
                _ => {}
            }
        }
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
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let qualified = qname(st, scope, &name);
    let tag = if is_method { "METHOD" } else { "FUNCTION" };

    let Some(idx) = out.push_node(
        tag,
        &name,
        node,
        format!("scala|{qualified}|{tag}|{params_text}"),
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
        is_definition: node.child_by_field_name("body").is_some(),
        node_index: Some(idx),
    });
    st.locals.push(name);

    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_value_def(st: &mut St<'_>, node: Node<'_>, scope: &[String], out: &mut Extraction<'_>) {
    let raw = text(node, st.src);
    let kind = first_identifier(node, st.src)
        .filter(|name| is_upper_const(name))
        .map(|_| SymbolKind::Constant)
        .unwrap_or(SymbolKind::Variable);

    let patterns = named_children(node);
    for child in patterns {
        if !out.tick() {
            return;
        }
        if child.kind() != "identifier" && child.kind() != "pattern" {
            continue;
        }
        if child.kind() == "identifier" {
            let name = text(child, st.src);
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
            continue;
        }
        for ident in named_children(child)
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
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let raw = text(node, st.src);
    let qualified = qname(st, scope, &name);
    let Some(idx) = out.push_node(
        "TYPE_ALIAS",
        &name,
        node,
        format!("scala|{qualified}|TYPE_ALIAS"),
        parent_idx,
    ) else {
        return;
    };
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: name.clone(),
        kind: SymbolKind::TypeAlias,
        span: span_of(node),
        is_public: is_public(&raw),
        disambiguator: None,
        signature: Some(raw),
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
    for clause in split_top_level(spec, ',') {
        emit_import_clause(node, clause.trim(), out);
    }
}

fn emit_import_clause(node: Node<'_>, clause: &str, out: &mut Extraction<'_>) {
    if clause.is_empty() {
        return;
    }
    if let Some(open) = clause.find('{') {
        let Some(close) = clause.rfind('}') else {
            return;
        };
        let base = clause[..open].trim().trim_end_matches('.');
        let inner = &clause[open + 1..close];
        for item in split_top_level(inner, ',') {
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            if item == "_" {
                out.push_import(format!("{base}.*"), "IMPORT", node);
                continue;
            }
            if let Some((name, _alias)) = item.split_once("=>") {
                let name = name.trim();
                if name != "_" {
                    out.push_import(format!("{base}.{name}"), "IMPORT", node);
                }
                continue;
            }
            out.push_import(format!("{base}.{item}"), "IMPORT", node);
        }
        return;
    }
    if let Some(base) = clause.strip_suffix("._") {
        out.push_import(format!("{base}.*"), "IMPORT", node);
        return;
    }
    out.push_import(clause.to_string(), "IMPORT", node);
}

fn extract_heritage(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_kind: OwnerKind,
    owner_sym: usize,
    out: &mut Extraction<'_>,
) {
    let Some(clause) =
        find_child_kind(node, "extend").or_else(|| find_child_kind(node, "extends_clause"))
    else {
        return;
    };
    let targets = extends_targets(clause, st.src);
    for (idx, target) in targets.into_iter().enumerate() {
        let rel = match owner_kind {
            OwnerKind::Interface => "EXTENDS",
            OwnerKind::Module | OwnerKind::Class => {
                if idx == 0 {
                    "EXTENDS"
                } else {
                    "IMPLEMENTS"
                }
            }
        };
        out.push_rel(
            rel,
            target,
            clause,
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
            let callee = node
                .child_by_field_name("function")
                .and_then(|n| last_identifier_text(n, st.src));
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
        "class_definition" | "trait_definition" | "object_definition" | "enum_definition" => {}
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

fn visibility_of(raw: &str) -> Option<String> {
    ["public", "protected", "private"]
        .into_iter()
        .find(|kw| contains_word(raw, kw))
        .map(ToOwned::to_owned)
}

fn is_public(raw: &str) -> bool {
    !contains_word(raw, "private") && !contains_word(raw, "protected")
}

fn contains_word(raw: &str, needle: &str) -> bool {
    raw.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|part| part == needle)
}

fn first_identifier(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    named_children(node)
        .into_iter()
        .find(|n| n.kind() == "identifier")
        .map(|n| text(n, src))
}

fn is_upper_const(name: &str) -> bool {
    name.chars().any(char::is_alphabetic)
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn extends_targets(node: Node<'_>, src: &SourceText<'_>) -> Vec<String> {
    let mut out = Vec::new();
    for child in named_children(node) {
        match child.kind() {
            "type_identifier" | "identifier" => out.push(text(child, src)),
            "generic_type" => {
                if let Some(name) = child
                    .child_by_field_name("type")
                    .or_else(|| child.child_by_field_name("name"))
                {
                    if let Some(target) = last_identifier_text(name, src) {
                        out.push(target);
                    }
                } else if let Some(target) = last_identifier_text(child, src) {
                    out.push(target);
                }
            }
            _ => {}
        }
    }
    out
}

fn last_identifier_text(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" => Some(text(node, src)),
        _ => {
            let mut c = node.walk();
            let kids: Vec<Node<'_>> = node.children(&mut c).collect();
            for ch in kids.into_iter().rev() {
                if let Some(found) = last_identifier_text(ch, src) {
                    return Some(found);
                }
            }
            None
        }
    }
}

fn split_top_level(text: &str, delim: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0u32;
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if ch == delim && depth == 0 => {
                parts.push(text[start..idx].trim().to_string());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim().to_string());
    parts
}
