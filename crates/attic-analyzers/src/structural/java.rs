//! Java language specification (grammar: `tree-sitter-java` 0.23.x).
//!
//! Extraction rules are grounded exclusively in parse trees inspected via
//! tests/parse_probe.rs against the pinned grammar — never copied from other
//! languages. Key observed kinds: `program`, `package_declaration`
//! (`scoped_identifier`), `import_declaration` (+ anonymous `static` / `*`
//! tokens), `class_declaration` / `interface_declaration` /
//! `enum_declaration` (`modifiers`, `superclass`, `super_interfaces` →
//! `type_list`), `method_declaration`, `constructor_declaration`,
//! `field_declaration` (`variable_declarator`), `method_invocation`,
//! `object_creation_expression`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static JAVA_SPEC: JavaSpec = JavaSpec;

pub struct JavaSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&JAVA_SPEC)
}

impl TreeSitterLanguageSpec for JavaSpec {
    fn analyzer_id(&self) -> &'static str {
        "java-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Java: structure, symbols \
         (classes/interfaces/enums, methods, constructors, fields), imports \
         and intra-file references with package-aware qualified names."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Java]
    }

    fn capabilities(&self) -> AnalyzerCapabilities {
        AnalyzerCapabilities {
            entries: vec![
                (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                (CapabilityKind::SymbolExtraction, CapabilityLevel::Full),
                (CapabilityKind::ImportExtraction, CapabilityLevel::Full),
                // Intra-file call/constructor references only; cross-file
                // resolution belongs to the indexing layer.
                (CapabilityKind::ReferenceExtraction, CapabilityLevel::Basic),
                (
                    CapabilityKind::RelationshipResolution,
                    CapabilityLevel::Basic,
                ),
            ],
        }
    }

    fn grammar(&self) -> tree_sitter_language::LanguageFn {
        tree_sitter_java::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "java"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            package: String::new(),
            locals: Vec::new(),
        };
        let scope: Vec<String> = Vec::new();
        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "package_declaration" => {
                    st.package = named_children(child)
                        .into_iter()
                        .next()
                        .map(|n| text(n, st.src))
                        .unwrap_or_default();
                }
                "import_declaration" => extract_import(child, st.src, out),
                "line_comment" | "block_comment" => {}
                _ => extract_type_decl(&mut st, child, &scope, None, out),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Walking state
// ---------------------------------------------------------------------------

struct St<'s> {
    src: &'s SourceText<'s>,
    package: String,
    /// Short names defined in this file — drives intra-file reference matching.
    locals: Vec<String>,
}

fn extract_type_decl(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let kind_tag = match node.kind() {
        "class_declaration" => "CLASS",
        "interface_declaration" => "INTERFACE",
        "enum_declaration" => "ENUM",
        _ => return,
    };
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);

    let modifiers = modifiers_of(node);
    let is_public = has_modifier(modifiers, "public");
    let visibility = visibility_of(modifiers);

    let mut qparts: Vec<String> = Vec::new();
    if !st.package.is_empty() {
        qparts.push(st.package.clone());
    }
    qparts.extend(scope.iter().cloned());
    qparts.push(name.clone());
    let qualified = qparts.join(".");

    let identity = format!("java|{qualified}|{kind_tag}");
    let Some(idx) = out.push_node(kind_tag, &name, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: name.clone(),
        kind: if kind_tag == "INTERFACE" {
            SymbolKind::Interface
        } else {
            // Enums model as classes in Phase 3 (no dedicated SymbolKind);
            // the enum nature is preserved in node_type.
            SymbolKind::Class
        },
        span: span_of(node),
        is_public,
        disambiguator: None,
        signature: None,
        visibility,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    // Heritage edges — syntax-level facts only (never claim resolution).
    if let Some(sc) = node.child_by_field_name("superclass") {
        let super_name = last_identifier_text(sc, st.src);
        if let Some(t) = super_name {
            out.push_rel(
                "EXTENDS",
                t,
                sc,
                ResolutionLevel::Syntactic,
                0.5,
                Some(sym_idx),
            );
        }
    }
    if let Some(si) = node.child_by_field_name("interfaces")
        && let Some(tl) = si.child_by_field_name("type_list").or_else(|| {
            named_children(si)
                .into_iter()
                .find(|n| n.kind() == "type_list")
        })
    {
        for t in named_children(tl) {
            let target = match t.kind() {
                "type_identifier" => Some(text(t, st.src)),
                // generic_type has NO named fields in this grammar version;
                // its base type is the first named child.
                "generic_type" => named_children(t)
                    .into_iter()
                    .next()
                    .map(|base| text(base, st.src)),
                _ => None,
            };
            if let Some(target) = target {
                out.push_rel(
                    "IMPLEMENTS",
                    target,
                    t,
                    ResolutionLevel::Syntactic,
                    0.5,
                    Some(sym_idx),
                );
            }
        }
    }

    let mut next_scope: Vec<String> = scope.to_vec();
    next_scope.push(name);
    if let Some(body) = node.child_by_field_name("body") {
        for member in named_children(body) {
            if !out.tick() {
                return;
            }
            match member.kind() {
                "class_declaration" | "interface_declaration" | "enum_declaration" => {
                    extract_type_decl(st, member, &next_scope, Some(idx), out);
                }
                "method_declaration" => {
                    extract_method(
                        st,
                        member,
                        &qualified_of(scope, &qparts),
                        false,
                        Some(idx),
                        out,
                    );
                }
                "constructor_declaration" => {
                    extract_method(
                        st,
                        member,
                        &qualified_of(scope, &qparts),
                        true,
                        Some(idx),
                        out,
                    );
                }
                "field_declaration" => extract_fields(st, member, &qualified, out),
                _ => {}
            }
        }
    }
    out.mark_top_level(idx);
}

fn qualified_of(_scope: &[String], qparts: &[String]) -> String {
    qparts.join(".")
}

fn extract_method(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    is_ctor: bool,
    owner_node: Option<usize>,
    out: &mut Extraction<'_>,
) {
    if !out.tick() {
        return;
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let modifiers = modifiers_of(node);
    let params = node.child_by_field_name("formal_parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();

    let signature = format!("{name}{params_text}");
    let qualified = format!("{owner_qualified}.{name}");
    let identity = format!("java|{qualified}|METHOD|{signature}");
    let Some(idx) = out.push_node(
        if is_ctor { "CONSTRUCTOR" } else { "METHOD" },
        &name,
        node,
        identity,
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
        is_public: has_modifier(modifiers, "public"),
        disambiguator: None, // overloads resolved deterministically post-pass
        signature: Some(signature),
        visibility: visibility_of(modifiers),
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_fields(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    out: &mut Extraction<'_>,
) {
    let modifiers = modifiers_of(node);
    let is_final = has_modifier(modifiers, "final");
    let decl_type = node
        .child_by_field_name("type")
        .map(|t| text(t, st.src))
        .unwrap_or_default();

    for decl in named_children(node) {
        if !out.tick() {
            return;
        }
        if decl.kind() != "variable_declarator" {
            continue;
        }
        let Some(name_node) = decl.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, st.src);
        out.push_symbol(CanonSymbol {
            qualified_name: format!("{owner_qualified}.{name}"),
            short_name: name.clone(),
            kind: if is_final {
                SymbolKind::Constant
            } else {
                SymbolKind::Variable
            },
            span: span_of(decl),
            is_public: has_modifier(modifiers, "public"),
            disambiguator: None,
            signature: Some(format!("{decl_type} {name}")),
            visibility: visibility_of(modifiers),
            is_definition: true,
            node_index: None,
        });
        st.locals.push(name);
    }
}

fn collect_calls(st: &mut St<'_>, body: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    match body.kind() {
        "method_invocation" => {
            if let Some(name_node) = body.child_by_field_name("name") {
                let callee = last_segment(text(name_node, st.src));
                if st.locals.contains(&callee) {
                    out.push_rel(
                        "CALL",
                        callee,
                        body,
                        ResolutionLevel::SymbolResolved,
                        0.9,
                        Some(owner_sym),
                    );
                }
            }
            recurse_children(st, body, owner_sym, out);
        }
        "object_creation_expression" => {
            if let Some(ty) = body.child_by_field_name("type") {
                let callee = last_segment(text(ty, st.src));
                if st.locals.contains(&callee) {
                    out.push_rel(
                        "CALL",
                        callee,
                        body,
                        ResolutionLevel::SymbolResolved,
                        0.9,
                        Some(owner_sym),
                    );
                }
            }
            recurse_children(st, body, owner_sym, out);
        }
        _ => recurse_children(st, body, owner_sym, out),
    }
}

fn recurse_children(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        collect_calls(st, ch, owner_sym, out);
    }
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn extract_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let mut is_static = false;
    let mut is_wildcard = false;
    let mut fqn: Option<String> = None;
    let mut c = node.walk();
    for ch in node.children(&mut c) {
        match ch.kind() {
            "scoped_identifier" | "identifier" => {
                if fqn.is_none() {
                    fqn = Some(text(ch, src));
                }
            }
            "static" => is_static = true,
            "*" => is_wildcard = true,
            _ => {}
        }
    }
    let Some(fqn) = fqn else { return };
    let raw = if is_wildcard { format!("{fqn}.*") } else { fqn };
    let kind = if is_static { "STATIC" } else { "IMPORT" };
    out.push_import(raw, kind, node);
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

fn modifiers_of(node: Node<'_>) -> Option<Node<'_>> {
    named_children(node)
        .into_iter()
        .find(|n| n.kind() == "modifiers")
}

fn has_modifier(modifiers: Option<Node<'_>>, kw: &str) -> bool {
    let Some(m) = modifiers else { return false };
    let mut c = m.walk();
    m.children(&mut c)
        .any(|ch| ch.kind() == kw && (!ch.is_named() || ch.kind() == kw))
}

fn visibility_of(modifiers: Option<Node<'_>>) -> Option<String> {
    for kw in ["public", "protected", "private"] {
        if has_modifier(modifiers, kw) {
            return Some(kw.to_string());
        }
    }
    None
}

fn last_segment(s: String) -> String {
    s.rsplit('.').next().unwrap_or(&s).to_string()
}

/// Last `type_identifier`/`identifier` descendant text (superclass nodes wrap
/// the name inside field wrappers).
fn last_identifier_text(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut result = None;
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" | "identifier" => result = Some(text(n, src)),
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
    result
}
