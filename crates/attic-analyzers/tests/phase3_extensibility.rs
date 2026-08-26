//! Phase 3 — architecture tests (§10 of the clarification).
//!
//! Proves the structural layer is genuinely language-agnostic:
//! - a NEW language registers through the generic registry with ZERO changes
//!   to registry/dispatch code;
//! - a language can exist WITHOUT reference resolution (capabilities are
//!   independent);
//! - one language's rules cannot affect another language;
//! - a panicking/malformed analyzer cannot destabilize others and
//!   GenericAnalyzer remains universally available;
//! - parser-specific types never leak into canonical persistence APIs
//!   (verified structurally against crate dependency manifests).

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerInput,
    AnalyzerOutput, AnalyzerRegistry, CancellationToken, CapabilityKind, CapabilityLevel,
    GenericAnalyzer, ResourceBudget, dispatch,
};
use attic_core::FileOccurrenceId;
use attic_core::FileType;

// ── A mock "new language" implemented WITHOUT tree-sitter ───────────────────

/// Simulates a future language using an arbitrary parser backend. It only
/// implements `Analyzer` — exactly like GenericAnalyzer — demonstrating that
/// structural support is NOT hard-wired to Tree-sitter or to any registry
/// modification.
struct MockLanguageAnalyzer {
    descriptor: AnalyzerDescriptor,
}

impl MockLanguageAnalyzer {
    fn new(ft: FileType) -> Self {
        Self {
            descriptor: AnalyzerDescriptor {
                name: "mock-lang".to_string(),
                version: "0.1.0".to_string(),
                description: "mock language proving extensibility".to_string(),
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

impl Analyzer for MockLanguageAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.descriptor
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        // Minimal but REAL canonical output.
        AnalyzerOutput {
            analyzer_id: self.descriptor.name.clone(),
            analyzer_version: self.descriptor.version.clone(),
            file_occurrence_id: input.file_occurrence_id,
            structural_nodes: vec![],
            symbols: vec![attic_analyzers::SymbolSpec {
                qualified_name: "mock::entry".to_string(),
                short_name: "entry".to_string(),
                kind: attic_core::SymbolKind::Function,
                definition_span: attic_core::SourceSpan::new(0, 0, 1, 10),
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
                span: attic_core::SourceSpan::new(0, 0, 1, 10),
                retrieval_text: "mock entry token".to_string(),
                ordinal: 0,
                structural_node_index: None,
            }],
            diagnostics: vec![],
            fallback_used: false,
            capability_used: CapabilityKind::SymbolExtraction,
        }
    }
}

fn input(code: &'static str, ft: FileType) -> AnalyzerInput {
    AnalyzerInput {
        file_occurrence_id: FileOccurrenceId::new_v4(),
        path: PathBuf::from("x.src"),
        content: AnalyzerContent::FullBytes(code.as_bytes().to_vec()),
        language_hint: None,
        file_type: ft,
        size_bytes: code.len() as u64,
        is_partial_scan: false,
        cancellation_token: CancellationToken::new(),
        resource_budget: ResourceBudget::default(),
    }
}

// ── §10-1/2: registration through the generic registry, no dispatch change ──

#[test]
fn new_language_registers_and_dispatches_without_central_changes() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(Arc::new(MockLanguageAnalyzer::new(FileType::Yaml)));

    let out = dispatch(&reg, input("entry: true", FileType::Yaml));
    assert_eq!(out.analyzer_id, "mock-lang");
    assert!(!out.fallback_used);
    assert!(out.symbols.iter().any(|s| s.short_name == "entry"));
}

// ── §10-3: unsupported language falls back to GenericAnalyzer ───────────────

#[test]
fn unsupported_language_falls_back_to_generic() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(Arc::new(MockLanguageAnalyzer::new(FileType::Yaml)));
    let _ = &mut reg; // registry intentionally has NO Java analyzer

    let out = dispatch(&reg, input("public class X {}", FileType::Java));
    assert_eq!(out.analyzer_id, "generic");
    assert!(!out.retrieval_units.is_empty(), "file stays searchable");
}

// ── §10-4: capabilities independently represented ───────────────────────────

#[test]
fn capabilities_are_independent_not_ordinal() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);

    // Language A: structure+symbols, explicitly NO references.
    struct NoRefs(AnalyzerDescriptor);
    impl Analyzer for NoRefs {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.0
        }
        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            AnalyzerOutput {
                analyzer_id: "norefs".into(),
                analyzer_version: "0".into(),
                file_occurrence_id: input.file_occurrence_id,
                structural_nodes: vec![],
                symbols: vec![],
                imports: vec![],
                relationships: vec![], // ← references absent BY DECLARATION too
                retrieval_units: vec![],
                diagnostics: vec![],
                fallback_used: false,
                capability_used: CapabilityKind::StructuralParse,
            }
        }
    }
    impl NoRefs {
        fn descriptor_value() -> AnalyzerDescriptor {
            AnalyzerDescriptor {
                name: "norefs".to_string(),
                version: "0".to_string(),
                description: String::new(),
                supported_file_types: vec![FileType::Json],
                capabilities: AnalyzerCapabilities {
                    entries: vec![
                        (CapabilityKind::StructuralParse, CapabilityLevel::Full),
                        (CapabilityKind::ReferenceExtraction, CapabilityLevel::None),
                    ],
                },
            }
        }
    }

    reg.register_specialized(Arc::new(NoRefs(NoRefs::descriptor_value())));
    let selected = reg.select(FileType::Json).0;
    let caps = selected.descriptor().capabilities.clone();
    assert_eq!(
        caps.level_for(CapabilityKind::StructuralParse),
        CapabilityLevel::Full
    );
    assert_eq!(
        caps.level_for(CapabilityKind::ReferenceExtraction),
        CapabilityLevel::None,
        "structure WITHOUT resolution must be declarable"
    );
}

// ── §10-6: one language cannot affect another ───────────────────────────────

#[test]
fn malformed_input_in_one_language_cannot_destabilize_another() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());
    reg.register_specialized(attic_analyzers::structural::python::analyzer());

    // Poison Python input; Java must still work perfectly afterwards.
    let py_out = dispatch(
        &reg,
        input("def broken(:\n  ??? ??\n\x01\x02\x03", FileType::Python),
    );
    let java_out = dispatch(&reg, input("public class Fine { int ok; }", FileType::Java));

    assert_eq!(py_out.analyzer_id, "python-treesitter");
    assert_eq!(java_out.analyzer_id, "java-treesitter");
    assert!(java_out.symbols.iter().any(|s| s.short_name == "Fine"));
}

// ── §10-9: GenericAnalyzer available regardless of registered languages ─────

#[test]
fn generic_remains_terminal_fallback_alongside_structural_registry() {
    let reg = attic_analyzers::default_registry();
    // Unknown textual type still routes to generic and stays searchable.
    let out = dispatch(&reg, input("plain text content", FileType::Text));
    assert_eq!(out.analyzer_id, "generic");
    assert!(
        out.retrieval_units
            .iter()
            .any(|u| u.retrieval_text.contains("plain text"))
    );
}

// ── §6/§7: parser-specific objects do not leak into canonical APIs ─────────

#[test]
fn tree_sitter_never_becomes_a_dependency_of_canonical_layers() {
    // Structural proof at the manifest level: ONLY attic-analyzers may depend
    // on tree-sitter crates. Storage / indexing / retrieval / server /
    // incremental must depend on the CANONICAL model instead.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");
    for crate_dir in [
        "crates/attic-storage",
        "crates/attic-indexing",
        "crates/attic-retrieval",
        "crates/attic-server",
        "crates/attic-incremental",
    ] {
        let manifest = std::fs::read_to_string(workspace.join(crate_dir).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("{crate_dir}: {e}"));
        assert!(
            !manifest.contains("tree-sitter"),
            "{crate_dir} must not depend on tree-sitter directly"
        );
    }
}
