//! Dockerfile / Containerfile structural analyzer.
//!
//! Dockerfiles do not have classes or callable symbols, but they do expose a
//! useful stage graph (`FROM ... AS stage`, `FROM <stage>`, `COPY --from=`),
//! repo-relative file dependencies (`COPY` / `ADD`) and build variables
//! (`ARG`, `ENV`). This adapter records only those facts and stays explicit
//! about the low ceiling of relationship coverage.

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

pub(crate) static DOCKERFILE_SPEC: DockerfileSpec = DockerfileSpec;

pub struct DockerfileSpec;

pub fn analyzer() -> Arc<dyn Analyzer> {
    make_analyzer(&DOCKERFILE_SPEC)
}

impl TreeSitterLanguageSpec for DockerfileSpec {
    fn analyzer_id(&self) -> &'static str {
        "dockerfile-treesitter"
    }

    fn description(&self) -> &'static str {
        "Tree-sitter structural analyzer for Dockerfile/Containerfile: build \
         stages, ARG/ENV variables, stage references and COPY/ADD file inputs."
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
                (CapabilityKind::ReferenceExtraction, CapabilityLevel::None),
                (
                    CapabilityKind::RelationshipResolution,
                    CapabilityLevel::Basic,
                ),
            ],
        }
    }

    fn grammar(&self) -> tree_sitter_language::LanguageFn {
        tree_sitter_containerfile::LANGUAGE
    }

    fn language_tag(&self) -> &'static str {
        "dockerfile"
    }

    fn extract(&self, root: Node<'_>, src: &SourceText<'_>, out: &mut Extraction<'_>) {
        let mut st = St {
            src,
            stages: HashMap::new(),
            current_stage: None,
        };
        for child in named_children(root) {
            if !out.tick() {
                return;
            }
            match child.kind() {
                "from_instruction" => extract_from(&mut st, child, out),
                "arg_instruction" => extract_arg(&mut st, child, out),
                "env_instruction" => extract_env(&mut st, child, out),
                "copy_instruction" => extract_copy_add(&mut st, child, "COPY", out),
                "add_instruction" => extract_copy_add(&mut st, child, "ADD", out),
                "healthcheck_instruction" => {
                    extract_simple_node(child, "HEALTHCHECK", "healthcheck", out)
                }
                "onbuild_instruction" => extract_onbuild(&mut st, child, out),
                _ => {}
            }
        }
    }
}

struct St<'s> {
    src: &'s SourceText<'s>,
    stages: HashMap<String, usize>,
    current_stage: Option<usize>,
}

fn extract_from(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    let base = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "image_spec")
        .map(|n| text(n, st.src).trim().to_string())
        .unwrap_or_default();
    let alias = node
        .child_by_field_name("as")
        .map(|n| text(n, st.src).trim().to_string())
        .filter(|name| !name.is_empty());

    let owner_sym = alias.as_ref().and_then(|name| st.stages.get(name).copied());
    if let Some(alias) = alias {
        let Some(idx) = out.push_node(
            "STAGE",
            &alias,
            node,
            format!("dockerfile|{alias}|STAGE"),
            None,
        ) else {
            return;
        };
        let sym_idx = out.symbols.len();
        out.push_symbol(CanonSymbol {
            qualified_name: alias.clone(),
            short_name: alias.clone(),
            kind: SymbolKind::Module,
            span: span_of(node),
            is_public: true,
            disambiguator: None,
            signature: (!base.is_empty()).then_some(base.clone()),
            visibility: None,
            is_definition: true,
            node_index: Some(idx),
        });
        out.mark_top_level(idx);
        st.stages.insert(alias, sym_idx);
        st.current_stage = Some(sym_idx);

        if !base.is_empty() && st.stages.contains_key(&base) {
            out.push_rel(
                "EXTENDS",
                base,
                node,
                ResolutionLevel::SymbolResolved,
                0.95,
                Some(sym_idx),
            );
        }
        return;
    }

    st.current_stage = owner_sym;
}

fn extract_arg(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    for pair in named_children(node) {
        let raw = text(pair, st.src);
        let name = raw.split('=').next().map(str::trim).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if let Some(idx) = out.push_node("ARG", name, pair, format!("dockerfile|{name}|ARG"), None)
        {
            out.push_symbol(CanonSymbol {
                qualified_name: name.to_string(),
                short_name: name.to_string(),
                kind: SymbolKind::Variable,
                span: span_of(pair),
                is_public: true,
                disambiguator: None,
                signature: None,
                visibility: None,
                is_definition: true,
                node_index: Some(idx),
            });
            out.mark_top_level(idx);
        }
    }
}

fn extract_env(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    for pair in named_children(node) {
        if pair.kind() != "env_pair" {
            continue;
        }
        let Some(name_node) = pair.child_by_field_name("name") else {
            continue;
        };
        let name = text(name_node, st.src);
        if name.is_empty() {
            continue;
        }
        if let Some(idx) = out.push_node("ENV", &name, pair, format!("dockerfile|{name}|ENV"), None)
        {
            out.push_symbol(CanonSymbol {
                qualified_name: name.clone(),
                short_name: name,
                kind: SymbolKind::Variable,
                span: span_of(pair),
                is_public: true,
                disambiguator: None,
                signature: None,
                visibility: None,
                is_definition: true,
                node_index: Some(idx),
            });
            out.mark_top_level(idx);
        }
    }
}

fn extract_copy_add(st: &mut St<'_>, node: Node<'_>, import_kind: &str, out: &mut Extraction<'_>) {
    let from_stage = named_children(node)
        .into_iter()
        .filter(|n| n.kind() == "param")
        .find_map(|param| {
            let raw = text(param, st.src).trim().to_string();
            raw.strip_prefix("--from=")
                .or_else(|| raw.strip_prefix("from="))
                .map(str::trim)
                .map(str::to_string)
                .or_else(|| st.stages.contains_key(raw.as_str()).then_some(raw))
        });

    if let Some(stage) = from_stage {
        if st.stages.contains_key(&stage) {
            out.push_rel(
                "REFERENCES",
                stage,
                node,
                ResolutionLevel::SymbolResolved,
                0.95,
                st.current_stage,
            );
        }
        return;
    }

    let sources = json_sources(node, st.src).unwrap_or_else(|| path_sources(node, st.src));
    for source in sources {
        if !source.is_empty() {
            out.push_import(source, import_kind, node);
        }
    }
}

fn extract_onbuild(st: &mut St<'_>, node: Node<'_>, out: &mut Extraction<'_>) {
    extract_simple_node(node, "ONBUILD", "onbuild", out);
    for child in named_children(node) {
        if !out.tick() {
            return;
        }
        match child.kind() {
            "copy_instruction" => extract_copy_add(st, child, "COPY", out),
            "add_instruction" => extract_copy_add(st, child, "ADD", out),
            "from_instruction" => extract_from(st, child, out),
            "healthcheck_instruction" => {
                extract_simple_node(child, "HEALTHCHECK", "healthcheck", out)
            }
            _ => {}
        }
    }
}

fn extract_simple_node(node: Node<'_>, tag: &str, name: &str, out: &mut Extraction<'_>) {
    if let Some(idx) = out.push_node(
        tag,
        name,
        node,
        format!("dockerfile|{tag}|{}", span_of(node)),
        None,
    ) {
        out.mark_top_level(idx);
    }
}

fn json_sources(node: Node<'_>, src: &SourceText<'_>) -> Option<Vec<String>> {
    let array = named_children(node)
        .into_iter()
        .find(|n| n.kind() == "json_string_array")?;
    let mut strings: Vec<String> = named_children(array)
        .into_iter()
        .map(|s| trim_quotes(&text(s, src)))
        .collect();
    if strings.len() < 2 {
        return Some(Vec::new());
    }
    strings.pop();
    Some(strings)
}

fn path_sources(node: Node<'_>, src: &SourceText<'_>) -> Vec<String> {
    let mut paths: Vec<String> = named_children(node)
        .into_iter()
        .filter(|n| n.kind() == "path")
        .map(|p| text(p, src).trim().to_string())
        .collect();
    if paths.len() < 2 {
        return Vec::new();
    }
    paths.pop();
    paths
}

fn trim_quotes(raw: &str) -> String {
    raw.trim().trim_matches('"').trim_matches('\'').to_string()
}

fn named_children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut c = node.walk();
    node.children(&mut c).filter(|n| n.is_named()).collect()
}

fn text(node: Node<'_>, src: &SourceText<'_>) -> String {
    src.text(node.start_byte(), node.end_byte())
}
