//! Phase 3 correction tests — resource-ceiling reconciliation, explicit
//! partial-structure advertisement, and registry-level extensibility.
//!
//! Covers review items 3, 4, and 5:
//! 1. Recursion-depth ceiling is a HARD fatal limit (protects recursive
//!    extractors) and is enforced against `ResourceBudget.max_recursion_depth`.
//! 2. Entity ceilings derive from `ResourceBudget.max_retrieval_units`;
//!    truncation is observable (warning diagnostics + PARTIAL marker) and
//!    never presented as complete.
//! 3. LARGE-file prefix parsing advertises `structurally_complete = false`.
//! 4. A mock structural language composes onto the REAL default registry
//!    externally; central dispatch contains no per-language branching.

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerInput,
    AnalyzerOutput, AnalyzerRegistry, CancellationToken, CapabilityKind, CapabilityLevel,
    GenericAnalyzer, ResourceBudget, dispatch,
};
use attic_core::{FileOccurrenceId, FileType, SourceSpan, SymbolKind};

fn input_with_budget(code: String, ft: FileType, budget: ResourceBudget) -> AnalyzerInput {
    let size = code.len() as u64;
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("x.src"),
        content: AnalyzerContent::FullBytes(code.into_bytes()),
        language_hint: None,
        file_type: ft,
        size_bytes: size,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: budget,
    }
}

// ── Fix 4a: depth ceiling is fatal and budget-driven ────────────────────────

#[test]
fn recursion_depth_ceiling_refuses_extraction_and_falls_back() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());

    // Nesting depth ~60 with max_recursion_depth = 20 → hard refusal.
    let depth = 60;
    let mut java = String::from("class Deep { int v = ");
    for _ in 0..depth {
        java.push('(');
    }
    java.push('1');
    for _ in 0..depth {
        java.push(')');
    }
    java.push_str("; }");

    let budget = ResourceBudget {
        max_recursion_depth: 20,
        ..Default::default()
    };
    let out = dispatch(&reg, input_with_budget(java, FileType::Java, budget));

    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.severity == attic_analyzers::DiagnosticSeverity::Error
                && d.code == "RESOURCE_EXHAUSTED"
                && d.message.contains("max_recursion_depth")),
        "depth refusal must be an explicit RESOURCE_EXHAUSTED error; got {:?}",
        out.diagnostics
            .iter()
            .map(|d| (&d.code, &d.message))
            .collect::<Vec<_>>()
    );
    assert!(
        out.fallback_used,
        "refused tree must route to GenericAnalyzer"
    );
    assert!(!out.retrieval_units.is_empty(), "file stays searchable");
    assert!(!out.structurally_complete);
}

#[test]
fn normal_depth_files_are_untouched_by_depth_ceiling() {
    let reg = attic_analyzers::default_registry();
    const SRC: &str = include_str!("fixtures/OrderService.java");
    let out = dispatch(
        &reg,
        input_with_budget(SRC.to_string(), FileType::Java, ResourceBudget::default()),
    );
    assert_eq!(out.analyzer_id, "java-treesitter");
    assert!(out.structurally_complete, "normal file must be COMPLETE");
    assert!(
        !out.diagnostics
            .iter()
            .any(|d| d.code == "RESOURCE_EXHAUSTED"),
        "no truncation diagnostics on a normal file"
    );
}

// ── Fix 4b: entity ceilings from max_retrieval_units are observable ─────────

#[test]
fn entity_cap_truncation_is_observable_and_never_claims_completeness() {
    let reg = attic_analyzers::default_registry();

    // Fixture defines several symbols/nodes; cap entities at 2.
    const SRC: &str = include_str!("fixtures/OrderService.java");
    let budget = ResourceBudget {
        max_retrieval_units: 2,
        ..Default::default()
    };
    let out = dispatch(
        &reg,
        input_with_budget(SRC.to_string(), FileType::Java, budget),
    );

    // Specialized output survives (warnings do NOT trigger fallback).
    assert_eq!(out.analyzer_id, "java-treesitter");
    assert!(!out.fallback_used);

    // Truncation is EXPLICITLY observable.
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "RESOURCE_EXHAUSTED"),
        "cap hit must emit RESOURCE_EXHAUSTED warning"
    );
    assert!(
        !out.structurally_complete,
        "truncated structural output must be marked PARTIAL"
    );

    // The effective ceiling was honoured (nodes + symbols bounded together).
    assert!(
        out.structural_nodes.len() <= 2 && out.symbols.len() <= 2,
        "entity caps applied: nodes={} symbols={}",
        out.structural_nodes.len(),
        out.symbols.len()
    );
}

// ── Fix 5: LARGE prefix parse advertises partial structure ──────────────────

#[test]
fn large_prefix_parse_advertises_partial_structure() {
    use attic_discovery::secrets::LargeFileStream;

    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());

    let filler = "x".repeat(6 * 1024);
    let unit = format!(
        "package big;\npublic class Chunk {{\n    public String blob() {{ return \"{filler}\"; }}\n}}\n\n"
    );
    let repeats = (5 * 1024 * 1024) / unit.len() + 1;
    let body: String = unit.repeat(repeats);

    let mut f = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write as _;
    std::io::Write::write_all(&mut f, body.as_bytes()).unwrap();
    f.flush().unwrap();
    let stream = LargeFileStream::open(f.path()).unwrap();

    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("big.java"),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Java,
            size_bytes: body.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        },
    );

    assert_eq!(out.analyzer_id, "java-treesitter");
    assert!(!out.structural_nodes.is_empty(), "prefix structure exists");
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "STRUCTURAL_TRUNCATED"),
        "prefix truncation diagnostic present"
    );
    // THE core §5 assertion: partial structure is never advertised complete.
    assert!(
        !out.structurally_complete,
        "prefix-parsed LARGE file must advertise structurally_complete=false"
    );
    // Lexical coverage preserved despite partial structure.
    let total_text: usize = out
        .retrieval_units
        .iter()
        .map(|u| u.retrieval_text.len())
        .sum();
    assert!(total_text >= body.len() - 4096);
}

// ── Fix 3: external composition on the REAL default registry ───────────────

/// Mock language proving third-party backends compose onto the production
/// registry WITHOUT touching central dispatch/registry code.
struct ThirdPartyLang {
    descriptor: AnalyzerDescriptor,
}

impl ThirdPartyLang {
    fn new(ft: FileType) -> Self {
        Self {
            descriptor: AnalyzerDescriptor {
                name: "third-party-lang".into(),
                version: "1.0.0".into(),
                description: "externally composed language".into(),
                supported_file_types: vec![ft],
                capabilities: AnalyzerCapabilities {
                    entries: vec![
                        (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                        (CapabilityKind::SymbolExtraction, CapabilityLevel::Full),
                    ],
                },
            },
        }
    }
}

impl Analyzer for ThirdPartyLang {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.descriptor
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        AnalyzerOutput {
            analyzer_id: self.descriptor.name.clone(),
            analyzer_version: self.descriptor.version.clone(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![attic_analyzers::SymbolSpec {
                qualified_name: "tp::root".into(),
                short_name: "root".into(),
                kind: SymbolKind::Module,
                definition_span: SourceSpan::new(0, 0, 1, 8),
                is_public: true,
                disambiguator: None,
                signature: None,
                visibility: None,
                is_definition: true,
                node_index: None,
            }],
            imports: vec![],
            relationships: vec![],
            retrieval_units: vec![attic_analyzers::RetrievalUnitSpec {
                span: SourceSpan::new(0, 0, 1, 8),
                retrieval_text: "third-party token".into(),
                ordinal: 0,
                structural_node_index: None,
            }],
            diagnostics: vec![],
            fallback_used: false,
            structurally_complete: true,
            capability_used: CapabilityKind::SymbolExtraction,
        }
    }
}

#[test]
fn third_party_language_composes_onto_default_registry_without_central_changes() {
    // Start from the REAL composition root…
    let mut reg = attic_analyzers::default_registry();
    // …and register an external language purely through the public API.
    reg.register_specialized(Arc::new(ThirdPartyLang::new(FileType::Yaml)));

    // External language dispatches through the SAME generic dispatcher.
    let tp = dispatch(
        &reg,
        input_with_budget("root: ok".into(), FileType::Yaml, ResourceBudget::default()),
    );
    assert_eq!(tp.analyzer_id, "third-party-lang");
    assert!(!tp.fallback_used);
    assert!(tp.symbols.iter().any(|s| s.short_name == "root"));
    assert!(tp.structurally_complete);

    // Built-in languages remain fully functional beside it (isolation).
    const JAVA_SRC: &str = include_str!("fixtures/OrderService.java");
    let java = dispatch(
        &reg,
        input_with_budget(
            JAVA_SRC.to_string(),
            FileType::Java,
            ResourceBudget::default(),
        ),
    );
    assert_eq!(java.analyzer_id, "java-treesitter");
    assert!(java.structurally_complete);

    // Unknown types still fall back to GenericAnalyzer.
    let unknown = dispatch(
        &reg,
        input_with_budget(
            "plain text".into(),
            FileType::Text,
            ResourceBudget::default(),
        ),
    );
    assert_eq!(unknown.analyzer_id, "generic");
}

/// Central dispatch/registry must contain NO per-language branching
/// (§8 of the clarification). Executable proof via source scan.
#[test]
fn central_dispatch_has_no_language_specific_branching() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for file in ["src/dispatch.rs", "src/registry.rs"] {
        let src = std::fs::read_to_string(manifest_dir.join(file))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        for lang in [
            "\"java\"",
            "\"python\"",
            "\"go\"",
            "\"javascript\"",
            "\"typescript\"",
        ] {
            assert!(
                !src.contains(lang),
                "{file} contains language-specific branching ({lang})"
            );
        }
    }
}
