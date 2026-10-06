//! Lua language specification (grammar: `tree-sitter-lua` 0.5.x).
//!
//! Grounded in parse trees produced by the pinned grammar. Key observed kinds:
//! `chunk`, `local_declaration`, `variable_declaration`,
//! `assignment_statement`, `function_declaration`, `function_call`,
//! `parameters`, `dot_index_expression`, `method_index_expression`,
//! `string` → `string_content`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static LUA_SPEC: LuaSpec = LuaSpec;

pub struct LuaSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&LUA_SPEC)
}

impl TreeSitterLanguageSpec for LuaSpec {
    fn analyzer_id(&self) -> &'static str {
        "lua-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Lua: structure, functions and \
         table methods, require()-based imports, and intra-file call edges."
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
        tree_sitter_lua::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "lua"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
        };

        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            extract_top_level_requires(child, st.src, out);
        }

        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "function_declaration" => extract_function(&mut st, child, true, out),
                "local_declaration" => extract_local_declaration(&mut st, child, out),
                _ => {}
            }
        }
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    locals: Vec<String>,
}

fn extract_local_declaration(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    for child in named_children(node) {
        if !out.tick() {
            return;
        }
        if child.kind() == "function_declaration" {
            extract_function(st, child, false, out);
        }
    }
}

fn extract_function(st: &mut St<'_>, node: Node<'_>, is_public: bool, out: &mut Extraction<'_>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let Some(name_parts) = function_name_parts(name_node, st.src) else {
        return;
    };
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let tag = if matches!(name_parts.kind, SymbolKind::Method) {
        "METHOD"
    } else {
        "FUNCTION"
    };

    let Some(idx) = out.push_node(
        tag,
        &name_parts.short_name,
        node,
        format!("lua|{}|{tag}|{params_text}", name_parts.qualified_name),
        None,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: name_parts.qualified_name.clone(),
        short_name: name_parts.short_name.clone(),
        kind: name_parts.kind,
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: Some(format!("{}{}", name_parts.short_name, params_text)),
        visibility: Some(if is_public {
            "public".to_string()
        } else {
            "local".to_string()
        }),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name_parts.short_name);
    out.mark_top_level(idx);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

struct FunctionName {
    qualified_name: String,
    short_name: String,
    kind: SymbolKind,
}

fn function_name_parts(node: Node<'_>, src: &SourceText<'_>) -> Option<FunctionName> {
    match node.kind() {
        "identifier" => {
            let name = text(node, src);
            Some(FunctionName {
                qualified_name: name.clone(),
                short_name: name,
                kind: SymbolKind::Function,
            })
        }
        "dot_index_expression" => {
            let table = node
                .child_by_field_name("table")
                .and_then(|n| last_identifier_text(n, src))?;
            let field = node
                .child_by_field_name("field")
                .and_then(|n| last_identifier_text(n, src))?;
            Some(FunctionName {
                qualified_name: format!("{table}.{field}"),
                short_name: field,
                kind: SymbolKind::Method,
            })
        }
        "method_index_expression" => {
            let table = node
                .child_by_field_name("table")
                .and_then(|n| last_identifier_text(n, src))?;
            let method = node
                .child_by_field_name("method")
                .and_then(|n| last_identifier_text(n, src))?;
            Some(FunctionName {
                qualified_name: format!("{table}.{method}"),
                short_name: method,
                kind: SymbolKind::Method,
            })
        }
        _ => None,
    }
}

fn extract_top_level_requires(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    if node.kind() == "function_declaration" {
        return;
    }
    if node.kind() == "function_call"
        && let Some(spec) = require_specifier(node, src)
    {
        out.push_import(spec, "REQUIRE", node);
        return;
    }
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        extract_top_level_requires(ch, src, out);
    }
}

fn require_specifier(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    if name.kind() != "identifier" || text(name, src) != "require" {
        return None;
    }
    let args = node.child_by_field_name("arguments")?;
    let mut stack = vec![args];
    while let Some(cur) = stack.pop() {
        if cur.kind() == "string_content" {
            return Some(text(cur, src));
        }
        let mut c = cur.walk();
        let kids: Vec<Node<'_>> = cur.children(&mut c).collect();
        for ch in kids.into_iter().rev() {
            stack.push(ch);
        }
    }
    None
}

fn collect_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    match node.kind() {
        "function_call" => {
            let callee = node
                .child_by_field_name("name")
                .and_then(|n| call_name(n, st.src));
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
        "function_declaration" => {}
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

fn call_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    match node.kind() {
        "identifier" => Some(text(node, src)),
        "dot_index_expression" => node
            .child_by_field_name("field")
            .and_then(|n| last_identifier_text(n, src)),
        "method_index_expression" => node
            .child_by_field_name("method")
            .and_then(|n| last_identifier_text(n, src)),
        _ => None,
    }
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
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
