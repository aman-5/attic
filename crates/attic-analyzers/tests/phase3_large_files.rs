//! Phase 3 — LARGE-file bounded structural handling (§11).
//!
//! A LARGE (>4 MiB) Java source arrives as `StreamingHandle`. The engine
//! must:
//! - parse only a bounded prefix for structure (never build a giant AST);
//! - emit `STRUCTURAL_TRUNCATED` so degradation is OBSERVABLE;
//! - still index the remaining bytes as lexical units (search coverage never
//!   regresses below GenericAnalyzer).

use std::path::PathBuf;
use std::sync::Arc;

use attic_analyzers::{
    Analyzer, AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResourceBudget, dispatch,
};
use attic_core::FileOccurrenceId;
use attic_core::FileType;
use attic_discovery::secrets::LargeFileStream;

#[test]
fn large_java_file_bounded_structure_with_full_text_coverage() {
    let mut reg = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
    reg.register_specialized(attic_analyzers::structural::java::analyzer());

    // Build a >4 MiB valid Java source whose AST stays well under
    // max_ast_nodes: few declarations, large filler strings.
    let filler = "x".repeat(6 * 1024);
    let unit = format!(
        "package big;\npublic class Chunk {{\n    public String blob() {{ return \"{filler}\"; }}\n}}\n\n"
    );
    let repeats = (5 * 1024 * 1024) / unit.len() + 1;
    let body: String = unit.repeat(repeats);

    let mut f = tempfile::NamedTempFile::new().unwrap();
    use std::io::Write;
    f.write_all(body.as_bytes()).unwrap();
    f.flush().unwrap();
    let stream = LargeFileStream::open(f.path()).unwrap();
    let size = body.len() as u64;

    let out = dispatch(
        &reg,
        AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("big.java"),
            content: AnalyzerContent::StreamingHandle(Box::new(stream)),
            language_hint: None,
            file_type: FileType::Java,
            size_bytes: size,
            is_partial_scan: false,
            cancellation_token: CancellationToken::new(),
            resource_budget: ResourceBudget::default(),
        },
    );

    assert_eq!(out.analyzer_id, "java-treesitter");
    assert!(!out.fallback_used);

    // Degradation is observable.
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.code == "STRUCTURAL_TRUNCATED"),
        "truncation must be reported"
    );

    // Bounded structure from the prefix.
    assert!(
        !out.structural_nodes.is_empty(),
        "prefix classes must be structurally parsed"
    );
    assert!(out.structural_nodes.len() < repeats as usize * 3);

    // Tail remains searchable: retrieval text must cover the whole file.
    let total_text: usize = out
        .retrieval_units
        .iter()
        .map(|u| u.retrieval_text.len())
        .sum();
    assert!(
        total_text >= body.len() - 4096,
        "lexical coverage must reach the whole file: {total_text} vs {}",
        body.len()
    );
}
