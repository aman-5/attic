//! Analyzer registry — deterministic selection of the best analyzer for a
//! given file type.
//!
//! Selection algorithm (from `docs/contracts/analyzers.md`):
//!
//! 1. Look up specialized analyzers registered for the detected `FileType`.
//!    If multiple match, prefer the one with the highest declared
//!    `CapabilityLevel` (via `AnalyzerCapabilities::max_level()`).
//!    Ties are broken by name (lexicographic ascending) for determinism.
//! 2. If no specialized analyzer matches, fall back to `GenericAnalyzer`.
//!
//! ## Capability selection semantics
//!
//! `CapabilityKind` variants are **independent dimensions** with no implicit
//! ordering between them.  Registry selection therefore compares
//! `CapabilityLevel` (the quality axis, ordered `None < Basic < Partial <
//! Full`) across whichever dimension the analyzer declares at its best level.
//! This avoids treating a high-ordinal `CapabilityKind` as universally
//! "better" than a low-ordinal one.

use std::collections::HashMap;
use std::sync::Arc;

use attic_core::FileType;
use tracing::debug;

use crate::api::{Analyzer, AnalyzerDescriptor, CapabilityLevel};

/// Entry stored in the registry for one registered analyzer.
struct RegistryEntry {
    analyzer: Arc<dyn Analyzer>,
    /// The highest `CapabilityLevel` this analyzer declares across all its
    /// supported capabilities.  Used for deterministic selection.
    max_level: CapabilityLevel,
}

/// Registry of all available analyzers.
///
/// Thread-safe; can be shared via `Arc<AnalyzerRegistry>`.
pub struct AnalyzerRegistry {
    /// Specialized analyzers keyed by the `FileType` they handle.
    ///
    /// Multiple analyzers may be registered for the same `FileType`.  When
    /// selecting, the one with the highest `max_level` wins; ties broken by
    /// `descriptor().name` (lexicographic ascending).
    specialized: HashMap<FileType, Vec<RegistryEntry>>,

    /// The mandatory language-agnostic fallback — always present.
    generic: Arc<dyn Analyzer>,
}

impl AnalyzerRegistry {
    /// Create a new registry with the given generic (fallback) analyzer.
    ///
    /// Additional specialized analyzers can be added via
    /// [`register_specialized`](Self::register_specialized).
    pub fn new(generic: Arc<dyn Analyzer>) -> Self {
        Self {
            specialized: HashMap::new(),
            generic,
        }
    }

    /// Register a specialized analyzer for all file types declared in its
    /// `AnalyzerDescriptor::supported_file_types`.
    ///
    /// If `supported_file_types` is empty (language-agnostic), the analyzer
    /// is not added to the specialized map — use it as the `generic` instead.
    ///
    /// # Panics
    ///
    /// Does not panic.  Invalid registrations are silently skipped with a
    /// `debug!` log.
    pub fn register_specialized(&mut self, analyzer: Arc<dyn Analyzer>) {
        let desc = analyzer.descriptor();
        if desc.supported_file_types.is_empty() {
            debug!(
                name = %desc.name,
                "register_specialized: analyzer has no supported_file_types; ignoring"
            );
            return;
        }

        let max_level = max_capability_level(desc);

        for ft in &desc.supported_file_types {
            let entry = RegistryEntry {
                analyzer: Arc::clone(&analyzer),
                max_level,
            };
            self.specialized.entry(*ft).or_default().push(entry);
        }
    }

    /// Select the best analyzer for the given `FileType`.
    ///
    /// Returns `(analyzer, is_generic)`:
    /// - `is_generic = false` → a specialized analyzer was selected.
    /// - `is_generic = true`  → the generic fallback was selected.
    ///
    /// Selection is **deterministic**: given the same registry state and file
    /// type, the same analyzer is always returned.
    pub fn select(&self, file_type: FileType) -> (Arc<dyn Analyzer>, bool) {
        if let Some(entries) = self.specialized.get(&file_type)
            && let Some(best) = best_entry(entries)
        {
            debug!(
                analyzer = %best.descriptor().name,
                "registry: selected specialized analyzer"
            );
            return (Arc::clone(best), false);
        }

        debug!("registry: no specialized analyzer; using generic");
        (Arc::clone(&self.generic), true)
    }

    /// Return the generic (fallback) analyzer directly.
    pub fn generic(&self) -> Arc<dyn Analyzer> {
        Arc::clone(&self.generic)
    }

    /// Return the descriptor of every registered analyzer (specialized +
    /// generic), sorted by name for determinism.  Useful for diagnostics.
    pub fn all_descriptors(&self) -> Vec<AnalyzerDescriptor> {
        let mut seen_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut out: Vec<AnalyzerDescriptor> = Vec::new();

        // Generic first.
        let gen_desc = self.generic.descriptor().clone();
        seen_names.insert(gen_desc.name.clone());
        out.push(gen_desc);

        // Then all specialized (deduplicated by name, since one analyzer may
        // be registered under multiple FileType keys).
        let mut specialized_descs: Vec<AnalyzerDescriptor> = self
            .specialized
            .values()
            .flat_map(|entries| entries.iter())
            .map(|e| e.analyzer.descriptor().clone())
            .filter(|d| seen_names.insert(d.name.clone()))
            .collect();

        specialized_descs.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(specialized_descs);
        out
    }
}

/// Return the analyzer with the highest `max_level`; break ties by name
/// (lexicographic ascending — "aaa" beats "zzz").
///
/// Returns `None` only if `entries` is empty.
fn best_entry(entries: &[RegistryEntry]) -> Option<&Arc<dyn Analyzer>> {
    entries
        .iter()
        .max_by(|a, b| {
            // Primary: higher CapabilityLevel wins.
            a.max_level.cmp(&b.max_level).then_with(|| {
                // Tie-break: lexicographically earlier name wins.
                // `max_by` returns the last "greatest" item; to get the
                // lexicographically smallest name we reverse the comparison.
                b.analyzer
                    .descriptor()
                    .name
                    .cmp(&a.analyzer.descriptor().name)
            })
        })
        .map(|e| &e.analyzer)
}

/// Compute the highest `CapabilityLevel` declared across all entries in an
/// `AnalyzerDescriptor`.  Returns `CapabilityLevel::None` for an empty
/// capabilities list (should not occur for well-formed descriptors).
fn max_capability_level(desc: &AnalyzerDescriptor) -> CapabilityLevel {
    desc.capabilities.max_level()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use attic_core::{FileOccurrenceId, FileType};

    use crate::api::{
        AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerInput, AnalyzerOutput,
        CapabilityKind, CapabilityLevel,
    };
    use crate::cancellation::CancellationToken;
    use crate::generic::GenericAnalyzer;

    // ── Minimal stub analyzer ───────────────────────────────────────────────

    struct StubAnalyzer {
        desc: AnalyzerDescriptor,
    }

    impl StubAnalyzer {
        fn new(
            name: &str,
            file_type: FileType,
            capability: CapabilityKind,
            level: CapabilityLevel,
        ) -> Self {
            Self {
                desc: AnalyzerDescriptor {
                    name: name.to_string(),
                    version: "0.1.0".to_string(),
                    description: "stub".to_string(),
                    supported_file_types: vec![file_type],
                    capabilities: AnalyzerCapabilities::single(capability, level),
                },
            }
        }
    }

    impl Analyzer for StubAnalyzer {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.desc
        }

        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            AnalyzerOutput {
                analyzer_id: self.desc.name.clone(),
                analyzer_version: self.desc.version.clone(),
                file_occurrence_id: input.file_occurrence_id,
                structural_nodes: vec![],
                symbols: vec![],
                imports: vec![],
                relationships: vec![],
                retrieval_units: vec![],
                diagnostics: vec![],
                fallback_used: false,
                structurally_complete: true,
                capability_used: CapabilityKind::Lexical,
            }
        }
    }

    fn make_input(ft: FileType, text: &str) -> AnalyzerInput {
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: std::path::PathBuf::from("test.rs"),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: None,
            file_type: ft,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: crate::api::ResourceBudget::default(),
        }
    }

    // ── Tests ───────────────────────────────────────────────────────────────

    /// AZ-08: Registry deterministically selects GenericAnalyzer for unknown type.
    #[test]
    fn registry_selects_generic_for_unknown_type() {
        let generic = Arc::new(GenericAnalyzer::new());
        let registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        let (selected, is_generic) = registry.select(FileType::Other);
        assert!(is_generic, "Other FileType must select generic");
        assert_eq!(selected.descriptor().name, "generic");
    }

    /// AZ-08: Registry selects specialized when registered.
    #[test]
    fn registry_selects_specialized_when_registered() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        let stub = Arc::new(StubAnalyzer::new(
            "rust-stub",
            FileType::Rust,
            CapabilityKind::SymbolExtraction,
            CapabilityLevel::Full,
        ));
        registry.register_specialized(stub as Arc<dyn Analyzer>);

        let (selected, is_generic) = registry.select(FileType::Rust);
        assert!(!is_generic, "Rust FileType must select specialized");
        assert_eq!(selected.descriptor().name, "rust-stub");
    }

    /// Selection is deterministic: higher CapabilityLevel wins regardless of CapabilityKind.
    #[test]
    fn registry_selection_is_deterministic() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        // Register two analyzers for Rust: one Full level, one Basic level.
        // Full must always win regardless of CapabilityKind dimension.
        let high = Arc::new(StubAnalyzer::new(
            "rust-high",
            FileType::Rust,
            CapabilityKind::Lexical, // lower-dimension kind
            CapabilityLevel::Full,   // but higher level
        ));
        let low = Arc::new(StubAnalyzer::new(
            "rust-low",
            FileType::Rust,
            CapabilityKind::SymbolExtraction, // higher-dimension kind
            CapabilityLevel::Basic,           // but lower level
        ));
        registry.register_specialized(high as Arc<dyn Analyzer>);
        registry.register_specialized(low as Arc<dyn Analyzer>);

        for _ in 0..10 {
            let (selected, is_generic) = registry.select(FileType::Rust);
            assert!(!is_generic);
            // Full > Basic → rust-high must always win
            assert_eq!(selected.descriptor().name, "rust-high");
        }
    }

    /// Capabilities are independent dimensions: CapabilityKind ordinal does NOT
    /// determine selection — only CapabilityLevel (quality) does.
    #[test]
    fn registry_selection_uses_capability_level_not_kind_ordinal() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        // "semantic-basic" declares the "highest" CapabilityKind (SemanticResolution)
        // but only at Basic level.
        // "lexical-full" declares the "lowest" CapabilityKind (Lexical) but at Full.
        // Full > Basic, so "lexical-full" must win.
        let semantic_basic = Arc::new(StubAnalyzer::new(
            "semantic-basic",
            FileType::Python,
            CapabilityKind::SemanticResolution,
            CapabilityLevel::Basic,
        ));
        let lexical_full = Arc::new(StubAnalyzer::new(
            "lexical-full",
            FileType::Python,
            CapabilityKind::Lexical,
            CapabilityLevel::Full,
        ));
        registry.register_specialized(semantic_basic as Arc<dyn Analyzer>);
        registry.register_specialized(lexical_full as Arc<dyn Analyzer>);

        let (selected, _) = registry.select(FileType::Python);
        assert_eq!(
            selected.descriptor().name,
            "lexical-full",
            "CapabilityLevel::Full must beat CapabilityLevel::Basic regardless of CapabilityKind"
        );
    }

    /// Tie-breaking by name (lexicographic): "aaa" beats "zzz" with equal level.
    #[test]
    fn registry_tie_broken_by_name_lexicographic() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        let a = Arc::new(StubAnalyzer::new(
            "aaa-rust",
            FileType::Rust,
            CapabilityKind::Lexical,
            CapabilityLevel::Full,
        ));
        let z = Arc::new(StubAnalyzer::new(
            "zzz-rust",
            FileType::Rust,
            CapabilityKind::Lexical,
            CapabilityLevel::Full,
        ));
        registry.register_specialized(z as Arc<dyn Analyzer>);
        registry.register_specialized(a as Arc<dyn Analyzer>);

        let (selected, _) = registry.select(FileType::Rust);
        assert_eq!(
            selected.descriptor().name,
            "aaa-rust",
            "lexicographically earlier name must win on tie"
        );
    }

    /// Registering analyzer with empty supported_file_types is silently ignored.
    #[test]
    fn register_language_agnostic_as_specialized_is_ignored() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        // Analyzer with no supported_file_types.
        let agnostic_desc = AnalyzerDescriptor {
            name: "agnostic".to_string(),
            version: "0.1.0".to_string(),
            description: "should be ignored".to_string(),
            supported_file_types: vec![],
            capabilities: AnalyzerCapabilities::single(
                CapabilityKind::Lexical,
                CapabilityLevel::Full,
            ),
        };
        struct AgnosticStub {
            desc: AnalyzerDescriptor,
        }
        impl Analyzer for AgnosticStub {
            fn descriptor(&self) -> &AnalyzerDescriptor {
                &self.desc
            }
            fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
                AnalyzerOutput {
                    analyzer_id: "agnostic".to_string(),
                    analyzer_version: "0.1.0".to_string(),
                    file_occurrence_id: input.file_occurrence_id,
                    structural_nodes: vec![],
                    symbols: vec![],
                    imports: vec![],
                    relationships: vec![],
                    retrieval_units: vec![],
                    diagnostics: vec![],
                    fallback_used: false,
                    structurally_complete: true,
                    capability_used: CapabilityKind::Lexical,
                }
            }
        }
        registry.register_specialized(Arc::new(AgnosticStub {
            desc: agnostic_desc,
        }) as Arc<dyn Analyzer>);

        // Still selects generic for any type.
        let (selected, is_generic) = registry.select(FileType::Rust);
        assert!(is_generic);
        assert_eq!(selected.descriptor().name, "generic");
    }

    /// all_descriptors returns unique names, sorted, with generic first.
    #[test]
    fn all_descriptors_returns_deduplicated_sorted() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        // Register analyzer for multiple file types (should appear once).
        struct MultiStub {
            desc: AnalyzerDescriptor,
        }
        impl Analyzer for MultiStub {
            fn descriptor(&self) -> &AnalyzerDescriptor {
                &self.desc
            }
            fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
                AnalyzerOutput {
                    analyzer_id: "multi".to_string(),
                    analyzer_version: "0.1.0".to_string(),
                    file_occurrence_id: input.file_occurrence_id,
                    structural_nodes: vec![],
                    symbols: vec![],
                    imports: vec![],
                    relationships: vec![],
                    retrieval_units: vec![],
                    diagnostics: vec![],
                    fallback_used: false,
                    structurally_complete: true,
                    capability_used: CapabilityKind::Lexical,
                }
            }
        }
        let multi_desc = AnalyzerDescriptor {
            name: "multi".to_string(),
            version: "0.1.0".to_string(),
            description: "multi".to_string(),
            supported_file_types: vec![FileType::Rust, FileType::TypeScript],
            capabilities: AnalyzerCapabilities::single(
                CapabilityKind::Lexical,
                CapabilityLevel::Full,
            ),
        };
        registry
            .register_specialized(Arc::new(MultiStub { desc: multi_desc }) as Arc<dyn Analyzer>);

        let descs = registry.all_descriptors();
        let names: Vec<&str> = descs.iter().map(|d| d.name.as_str()).collect();

        // Generic always first.
        assert_eq!(names[0], "generic");
        // multi appears exactly once.
        assert_eq!(names.iter().filter(|&&n| n == "multi").count(), 1);
    }

    /// AZ-08: Fallback-used pattern: registry returns generic, caller marks output.
    #[test]
    fn registry_generic_fallback_output_has_fallback_used_true() {
        let generic = Arc::new(GenericAnalyzer::new());
        let registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        // Simulate: specialized not found → use generic, mark fallback_used.
        let input = make_input(FileType::Rust, "fn main() {}");
        let (analyzer, is_generic) = registry.select(FileType::Rust);
        assert!(is_generic);

        let mut output = analyzer.analyze(input);
        // Caller responsibility: mark fallback when specialized was expected.
        output.fallback_used = true;

        assert!(output.fallback_used);
    }

    /// Partial level still beats None level in selection.
    #[test]
    fn registry_partial_beats_none_level() {
        let generic = Arc::new(GenericAnalyzer::new());
        let mut registry = AnalyzerRegistry::new(Arc::clone(&generic) as Arc<dyn Analyzer>);

        let partial = Arc::new(StubAnalyzer::new(
            "partial-analyzer",
            FileType::JavaScript,
            CapabilityKind::SymbolExtraction,
            CapabilityLevel::Partial,
        ));
        let basic = Arc::new(StubAnalyzer::new(
            "basic-analyzer",
            FileType::JavaScript,
            CapabilityKind::SymbolExtraction,
            CapabilityLevel::Basic,
        ));
        registry.register_specialized(basic as Arc<dyn Analyzer>);
        registry.register_specialized(partial as Arc<dyn Analyzer>);

        let (selected, _) = registry.select(FileType::JavaScript);
        assert_eq!(
            selected.descriptor().name,
            "partial-analyzer",
            "Partial level must beat Basic level"
        );
    }
}
