//! Swift language specification (grammar: `tree-sitter-swift` 0.7.x).
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `source_file`, `import_declaration`, `protocol_declaration`,
//! `class_declaration` with `declaration_kind` of `class` / `struct` /
//! `enum` / `extension`, `inheritance_specifier`, `function_declaration`,
//! `init_declaration`, `protocol_function_declaration`,
//! `call_expression`, `navigation_expression`, `simple_identifier`,
//! `user_type`, `type_identifier`.

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static SWIFT_SPEC: SwiftSpec = SwiftSpec;

pub struct SwiftSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&SWIFT_SPEC)
}

impl TreeSitterLanguageSpec for SwiftSpec {
    fn analyzer_id(&self) -> &'static str {
        "swift-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Swift: structure, symbols \
         (protocols, classes, structs, enums, extensions, functions, \
         initializers), module imports, inheritance/protocol conformance and \
         intra-file call edges."
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
        tree_sitter_swift::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "swift"
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
            "import_declaration" => extract_import(st, child, out),
            "protocol_declaration" => {
                extract_protocol(st, child, scope, parent_idx, top_level, out)
            }
            "class_declaration" => match declaration_kind(child, st.src).as_deref() {
                Some("extension") => {
                    extract_extension(st, child, scope, parent_idx, top_level, out)
                }
                Some("class" | "struct" | "enum") => {
                    extract_nominal_type(st, child, scope, parent_idx, top_level, out)
                }
                _ => {}
            },
            "function_declaration" => {
                extract_function(st, child, None, scope, parent_idx, top_level, false, out)
            }
            _ => {}
        }
    }
}

fn extract_import(st: &St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let Some(identifier) = named_children(node)
        .into_iter()
        .find(|child| child.kind() == "identifier")
    else {
        return;
    };
    out.push_import(text(identifier, st.src), "IMPORT", node);
}

fn extract_protocol(
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
    let name = normalize_type_name(&text(name_node, st.src));
    let short = last_segment(&name);
    let qualified = qname(scope, &name);
    let identity = format!("swift|{qualified}|PROTOCOL");
    let Some(idx) = out.push_node("PROTOCOL", &short, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: short.clone(),
        kind: SymbolKind::Interface,
        span: span_of(node),
        is_public: is_public(node),
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    for target in inheritance_targets(node, st.src) {
        out.push_rel(
            "EXTENDS",
            target,
            node,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }

    if let Some(body) = node.child_by_field_name("body") {
        for member in named_children(body) {
            if member.kind() == "protocol_function_declaration" {
                extract_protocol_method(st, member, &qualified, idx, out);
            }
        }
    }
}

fn extract_nominal_type(
    st: &mut St<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_idx: Option<usize>,
    top_level: bool,
    out: &mut Extraction<'_>,
) {
    let kind = declaration_kind(node, st.src).unwrap_or_default();
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = normalize_type_name(&text(name_node, st.src));
    let short = last_segment(&name);
    let qualified = qname(scope, &name);
    let tag = kind.to_ascii_uppercase();
    let identity = format!("swift|{qualified}|{tag}");
    let Some(idx) = out.push_node(&tag, &short, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: short.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: is_public(node),
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    let mut heritage = inheritance_targets(node, st.src).into_iter();
    if kind == "class"
        && let Some(base) = heritage.next()
    {
        out.push_rel(
            "EXTENDS",
            base,
            node,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }
    for target in heritage.filter(|target| !is_swift_raw_value_type(target)) {
        out.push_rel(
            "IMPLEMENTS",
            target,
            node,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }

    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let mut next_scope = scope.to_vec();
    next_scope.push(short);
    prime_local_names(st, body);
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "function_declaration" => extract_function(
                st,
                member,
                Some(&qualified),
                &next_scope,
                Some(idx),
                false,
                true,
                out,
            ),
            "init_declaration" => extract_init(st, member, &qualified, idx, out),
            "class_declaration" => match declaration_kind(member, st.src).as_deref() {
                Some("extension") => {
                    extract_extension(st, member, &next_scope, Some(idx), false, out)
                }
                Some("class" | "struct" | "enum") => {
                    extract_nominal_type(st, member, &next_scope, Some(idx), false, out)
                }
                _ => {}
            },
            "protocol_declaration" => {
                extract_protocol(st, member, &next_scope, Some(idx), false, out)
            }
            _ => {}
        }
    }
}

fn extract_extension(
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
    let name = normalize_type_name(&text(name_node, st.src));
    let short = last_segment(&name);
    let qualified = qname(scope, &name);
    let identity = format!("swift|{qualified}|EXTENSION");
    let Some(idx) = out.push_node("EXTENSION", &short, node, identity, parent_idx) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified.clone(),
        short_name: short.clone(),
        kind: SymbolKind::Class,
        span: span_of(node),
        is_public: is_public(node),
        disambiguator: None,
        signature: None,
        visibility: None,
        is_definition: false,
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    out.push_rel(
        "EXTENDS",
        qualified.clone(),
        node,
        ResolutionLevel::Syntactic,
        0.5,
        Some(sym_idx),
    );
    for target in inheritance_targets(node, st.src) {
        out.push_rel(
            "IMPLEMENTS",
            target,
            node,
            ResolutionLevel::Syntactic,
            0.5,
            Some(sym_idx),
        );
    }

    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    prime_local_names(st, body);
    for member in named_children(body) {
        if !out.tick() {
            return;
        }
        match member.kind() {
            "function_declaration" => extract_function(
                st,
                member,
                Some(&qualified),
                scope,
                Some(idx),
                false,
                true,
                out,
            ),
            "init_declaration" => extract_init(st, member, &qualified, idx, out),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_function(
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
    let qualified = owner_qualified
        .map(|owner| format!("{owner}.{name}"))
        .unwrap_or_else(|| qname(scope, &name));
    let body = node.child_by_field_name("body");
    let tag = if is_member { "METHOD" } else { "FUNCTION" };
    let signature = signature_text(node, body, st.src).unwrap_or_default();
    let identity = format!("swift|{qualified}|{tag}|{signature}");
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
        is_public: is_public(node),
        disambiguator: None,
        signature: (!signature.is_empty()).then_some(signature.clone()),
        visibility: None,
        is_definition: body.is_some(),
        node_index: Some(idx),
    });
    if top_level {
        out.mark_top_level(idx);
    }

    if let Some(body) = body {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_init(
    st: &mut St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_idx: usize,
    out: &mut Extraction<'_>,
) {
    let body = node.child_by_field_name("body");
    let signature = signature_text(node, body, st.src).unwrap_or_else(|| "init".to_string());
    let qualified = format!("{owner_qualified}.init");
    let identity = format!("swift|{qualified}|INIT|{signature}");
    let Some(idx) = out.push_node("INIT", "init", node, identity, Some(owner_idx)) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qualified,
        short_name: "init".to_string(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: is_public(node),
        disambiguator: None,
        signature: Some(signature),
        visibility: None,
        is_definition: body.is_some(),
        node_index: Some(idx),
    });

    if let Some(body) = body {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_protocol_method(
    st: &St<'_>,
    node: Node<'_>,
    owner_qualified: &str,
    owner_idx: usize,
    out: &mut Extraction<'_>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let signature = text(node, st.src);
    out.push_symbol(CanonSymbol {
        qualified_name: format!("{owner_qualified}.{name}"),
        short_name: name,
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: true,
        disambiguator: None,
        signature: Some(signature),
        visibility: None,
        is_definition: false,
        node_index: Some(owner_idx),
    });
}

fn collect_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    if node.kind() == "call_expression"
        && let Some(expr) = named_children(node).into_iter().next()
        && let Some((short, display)) = call_target(expr, st.src)
    {
        let (target, resolution, confidence) = if st.locals.contains(&short) {
            (short, ResolutionLevel::SymbolResolved, 0.85)
        } else {
            let syntactic = if display.contains('.') { 0.7 } else { 0.6 };
            (display, ResolutionLevel::Syntactic, syntactic)
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
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for child in kids {
        collect_calls(st, child, owner_sym, out);
    }
}

fn call_target(node: Node<'_>, src: &SourceText<'_>) -> Option<(String, String)> {
    match node.kind() {
        "simple_identifier" | "type_identifier" => {
            let name = text(node, src);
            Some((name.clone(), name))
        }
        "user_type" => {
            let name = normalize_type_name(&text(node, src));
            let short = last_segment(&name);
            Some((short, name))
        }
        "navigation_expression" => {
            let target = node.child_by_field_name("target")?;
            let suffix = first_suffix_name(node, src)?;
            let display = format!("{}.{}", normalize_type_name(&text(target, src)), suffix);
            Some((suffix, display))
        }
        _ => None,
    }
}

fn first_suffix_name(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        if next.kind() == "simple_identifier" && next != node {
            return Some(text(next, src));
        }
        let mut c = next.walk();
        for child in next.children(&mut c) {
            stack.push(child);
        }
    }
    None
}

fn prime_local_names(st: &mut St<'_>, container: Node<'_>) {
    for child in named_children(container) {
        let candidate = match child.kind() {
            "protocol_declaration" | "function_declaration" => child
                .child_by_field_name("name")
                .map(|name| text(name, st.src)),
            "class_declaration" => child
                .child_by_field_name("name")
                .map(|name| last_segment(&normalize_type_name(&text(name, st.src)))),
            "init_declaration" => Some("init".to_string()),
            _ => None,
        };
        if let Some(name) = candidate
            && !st.locals.contains(&name)
        {
            st.locals.push(name);
        }
    }
}

fn inheritance_targets(node: Node<'_>, src: &SourceText<'_>) -> Vec<String> {
    named_children(node)
        .into_iter()
        .filter(|child| child.kind() == "inheritance_specifier")
        .filter_map(|child| child.child_by_field_name("inherits_from"))
        .map(|target| normalize_type_name(&text(target, src)))
        .collect()
}

fn signature_text(node: Node<'_>, body: Option<Node<'_>>, src: &SourceText<'_>) -> Option<String> {
    let end = body
        .map(|body| body.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let text = src.text(node.start_byte(), end).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn declaration_kind(node: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    node.child_by_field_name("declaration_kind")
        .map(|kind| text(kind, src))
}

fn is_public(node: Node<'_>) -> bool {
    contains_token(node, &["public", "open"])
}

fn contains_token(node: Node<'_>, targets: &[&str]) -> bool {
    let mut stack = vec![node];
    while let Some(next) = stack.pop() {
        if targets.iter().any(|target| next.kind() == *target) {
            return true;
        }
        let mut c = next.walk();
        for child in next.children(&mut c) {
            stack.push(child);
        }
    }
    false
}

fn is_swift_raw_value_type(target: &str) -> bool {
    matches!(
        target,
        "String" | "Int" | "UInt" | "Double" | "Float" | "Bool" | "Character"
    )
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

fn normalize_type_name(raw: &str) -> String {
    raw.trim()
        .split('<')
        .next()
        .unwrap_or(raw)
        .trim()
        .to_string()
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
