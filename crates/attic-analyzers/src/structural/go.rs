//! Go language specification (grammar: `tree-sitter-go` 0.25.x).
//!
//! Grounded in probe output for this grammar version. Key observed kinds:
//! `source_file`, `package_clause` (`package_identifier`),
//! `import_declaration` (`import_spec_list` → `import_spec`, or a bare
//! `import_spec`; path via `interpreted_string_literal_content`; optional
//! alias as `package_identifier`), `function_declaration`,
//! `method_declaration` (receiver `parameter_list`, name is
//! `field_identifier`), `type_declaration` → `type_spec`
//! (`struct_type` / `interface_type`) and `type_alias`,
//! `const_declaration`/`var_declaration` (`const_spec`/`var_spec`),
//! `call_expression` (+ `selector_expression`).

use std::sync::Arc;

use attic_core::{FileType, SymbolKind};
use tree_sitter::Node;

use crate::api::{
    Analyzer, AnalyzerCapabilities, CapabilityKind, CapabilityLevel, ResolutionLevel,
};
use crate::structural::{
    CanonSymbol, Extraction, SourceText, TreeSitterLanguageSpec, make_analyzer, span_of,
};

pub(crate) static GO_SPEC: GoSpec = GoSpec;

pub struct GoSpec;

/// Public factory for registry wiring.
pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&GO_SPEC)
}

impl TreeSitterLanguageSpec for GoSpec {
    fn analyzer_id(&self) -> &'static str {
        "go-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Go: structure, symbols \
         (functions, methods with receivers, structs, interfaces, type \
         aliases, consts/vars), imports, and intra-file references. \
         Package context comes from the package clause; module-path \
         resolution happens in the indexing layer against go.mod."
    }

    fn file_types(&self) -> &'static [FileType] {
        &[FileType::Go]
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
                // Build resolution itself (module graph) lives in the
                // indexing layer; the analyzer records syntax facts only.
            ],
        }
    }

    fn grammar(&self) -> tree_sitter_language::LanguageFn {
        tree_sitter_go::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "go"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            locals: Vec::new(),
            package: String::new(),
        };

        // Pass 1 — declarations (package, imports, top-level decls).
        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "package_clause" => {
                    if let Some(pn) = named_children(child)
                        .into_iter()
                        .find(|n| n.kind() == "package_identifier")
                    {
                        st.package = text(pn, st.src);
                    }
                }
                "import_declaration" => extract_import(child, st.src, out),
                _ => {}
            }
        }

        // Pass 2 — declarations + intra-file references.
        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "function_declaration" => extract_function(&mut st, child, out),
                "method_declaration" => extract_method(&mut st, child, out),
                "type_declaration" => extract_type_decl(&mut st, child, out),
                "const_declaration" | "var_declaration" => extract_value_decl(&mut st, child, out),
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

fn qualified(st: &St<'_>, name: &str) -> String {
    if st.package.is_empty() {
        name.to_string()
    } else {
        format!("{}.{}", st.package, name)
    }
}

fn extract_function(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();
    let result = node.child_by_field_name("result");
    let result_text = result.map(|r| text(r, st.src)).unwrap_or_default();

    let signature = format!("{name}{params_text}{result_text}");
    let qname = qualified(st, &name);
    let identity = format!("go|{qname}|FUNCTION|{signature}");
    let Some(idx) = out.push_node("FUNCTION", &name, node, identity, None) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qname,
        short_name: name.clone(),
        kind: SymbolKind::Function,
        span: span_of(node),
        is_public: name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
        disambiguator: None,
        signature: Some(signature),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());
    out.mark_top_level(idx);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_method(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let name = text(name_node, st.src);
    let receiver_base = node
        .child_by_field_name("receiver")
        .and_then(|r| receiver_type_name(r, st.src))
        .unwrap_or_default();
    let params = node.child_by_field_name("parameters");
    let params_text = params.map(|p| text(p, st.src)).unwrap_or_default();

    // Convention: pkg.T.Name (value and pointer receivers share the name).
    let qname = format!("{}.{}.{}", st.package, receiver_base, name);
    let identity = format!("go|{qname}|METHOD|{params_text}");
    let Some(idx) = out.push_node(
        "METHOD",
        &format!("{receiver_base}.{name}"),
        node,
        identity,
        None,
    ) else {
        return;
    };

    let sym_idx = out.symbols.len();
    out.push_symbol(CanonSymbol {
        qualified_name: qname,
        short_name: name.clone(),
        kind: SymbolKind::Method,
        span: span_of(node),
        is_public: name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            || receiver_base
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase()),
        disambiguator: None,
        signature: Some(params_text.to_string()),
        visibility: None,
        is_definition: true,
        node_index: Some(idx),
    });
    st.locals.push(name.clone());
    out.mark_top_level(idx);

    if let Some(body) = node.child_by_field_name("body") {
        collect_calls(st, body, sym_idx, out);
    }
}

fn extract_type_decl(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    for spec in named_children(node) {
        if !out.tick() {
            return;
        }
        match spec.kind() {
            "type_spec" => {
                let Some(name_node) = spec.child_by_field_name("name") else {
                    continue;
                };
                let name = text(name_node, st.src);
                let ty = spec.child_by_field_name("type");
                let ty_kind = ty.map(|t| t.kind()).unwrap_or("");
                let (kind_symbol, tag) = match ty_kind {
                    "struct_type" => (SymbolKind::Class, "STRUCT"),
                    "interface_type" => (SymbolKind::Interface, "INTERFACE"),
                    _ => (SymbolKind::TypeAlias, "TYPE"),
                };
                let qname = qualified(st, &name);
                let identity = format!("go|{qname}|{tag}");
                let Some(idx) = out.push_node(tag, &name, spec, identity, None) else {
                    return;
                };
                out.push_symbol(CanonSymbol {
                    qualified_name: qname.clone(),
                    short_name: name.clone(),
                    kind: kind_symbol,
                    span: span_of(spec),
                    is_public: name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
                    disambiguator: None,
                    signature: None,
                    visibility: None,
                    is_definition: true,
                    node_index: Some(idx),
                });
                st.locals.push(name.clone());

                // Interface method elements become Method signatures
                // (is_definition=false — pure API surface).
                if ty_kind == "interface_type"
                    && let Some(ty_node) = ty
                {
                    for elem in named_children(ty_node) {
                        if !out.tick() {
                            return;
                        }
                        if elem.kind() == "method_elem" {
                            let m_name = named_children(elem)
                                .into_iter()
                                .find(|n| n.kind() == "field_identifier")
                                .map(|n| text(n, st.src))
                                .unwrap_or_default();
                            let m_qualified = format!("{qname}.{m_name}");
                            out.push_symbol(CanonSymbol {
                                qualified_name: m_qualified,
                                short_name: m_name.clone(),
                                kind: SymbolKind::Method,
                                span: span_of(elem),
                                is_public: true,
                                disambiguator: None,
                                signature: Some(text(elem, st.src)),
                                visibility: None,
                                is_definition: false,
                                node_index: Some(idx),
                            });
                        }
                    }
                }
                out.mark_top_level(idx);
            }
            "type_alias" => {
                let Some(name_node) = spec.child_by_field_name("name") else {
                    continue;
                };
                let name = text(name_node, st.src);
                let qname = qualified(st, &name);
                let Some(idx) =
                    out.push_node("TYPE_ALIAS", &name, spec, format!("go|{qname}|ALIAS"), None)
                else {
                    return;
                };
                out.push_symbol(CanonSymbol {
                    qualified_name: qname,
                    short_name: name.clone(),
                    kind: SymbolKind::TypeAlias,
                    span: span_of(spec),
                    is_public: name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
                    disambiguator: None,
                    signature: None,
                    visibility: None,
                    is_definition: true,
                    node_index: Some(idx),
                });
                st.locals.push(name);
                out.mark_top_level(idx);
            }
            _ => {}
        }
    }
}

fn extract_value_decl(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let is_const = node.kind() == "const_declaration";
    for spec in named_children(node) {
        if !out.tick() {
            return;
        }
        let spec_kind_ok = if is_const {
            spec.kind() == "const_spec"
        } else {
            spec.kind() == "var_spec" || spec.kind() == "var_assignment_statement"
        };
        if !spec_kind_ok {
            continue;
        }
        for name_node in named_children(spec)
            .into_iter()
            .filter(|n| n.kind() == "identifier")
        {
            let name = text(name_node, st.src);
            let qname = qualified(st, &name);
            out.push_symbol(CanonSymbol {
                qualified_name: qname,
                short_name: name.clone(),
                kind: if is_const {
                    SymbolKind::Constant
                } else {
                    SymbolKind::Variable
                },
                span: span_of(spec),
                is_public: name.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
                disambiguator: None,
                signature: None,
                visibility: None,
                is_definition: true,
                node_index: None,
            });
            st.locals.push(name);
        }
    }
}

// ---------------------------------------------------------------------------
// Imports
// ---------------------------------------------------------------------------

fn extract_import(node: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        match ch.kind() {
            "import_spec_list" => {
                for spec in named_children(ch) {
                    if spec.kind() == "import_spec" {
                        emit_import_spec(spec, src, out);
                    }
                }
            }
            "import_spec" => emit_import_spec(ch, src, out),
            _ => {}
        }
    }
}

fn emit_import_spec(spec: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
    // Grammar 0.25: fields `path` (interpreted_string_literal) and optional
    // alias `name` (package_identifier).
    let Some(path_node) = spec.child_by_field_name("path") else {
        return;
    };
    let content = named_children(path_node)
        .into_iter()
        .find(|n| n.kind() == "interpreted_string_literal_content");
    if let Some(content) = content {
        out.push_import(text(content, src), "IMPORT", spec);
    }
}

// ---------------------------------------------------------------------------
// Intra-file references
// ---------------------------------------------------------------------------

fn collect_calls(st: &mut St<'_>, node: Node<'_>, owner_sym: usize, out: &mut Extraction<'_>) {
    if !out.tick() {
        return;
    }
    if node.kind() == "call_expression"
        && let Some(func) = node.child_by_field_name("function")
        && func.kind() == "identifier"
    {
        let callee = text(func, st.src);
        if st.locals.contains(&callee) {
            out.push_rel(
                "CALL",
                callee,
                node,
                ResolutionLevel::SymbolResolved,
                0.9,
                Some(owner_sym),
            );
            return;
        }
    }
    let mut c = node.walk();
    let kids: Vec<Node<'_>> = node.children(&mut c).collect();
    for ch in kids {
        collect_calls(st, ch, owner_sym, out);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}

/// Base type name from a receiver parameter list: `(h *Handler)` → `Handler`.
fn receiver_type_name(receiver: Node<'_>, src: &SourceText<'_>) -> Option<String> {
    let mut stack = vec![receiver];
    while let Some(n) = stack.pop() {
        match n.kind() {
            "type_identifier" => return Some(text(n, src)),
            "parameter_declaration" | "pointer_type" | "generic_type" => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    stack.push(ch);
                }
            }
        }
    }
    None
}
