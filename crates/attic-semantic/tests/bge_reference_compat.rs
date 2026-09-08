//! BGE reference-compatibility test (Low-Level Design section 4 /
//! Verification section 6).
//!
//! Proves OUTPUT correctness, not just that the checkpoint loads: the
//! tokenize -> Candle BERT -> CLS-pool -> L2-normalize pipeline must
//! reproduce the canonical `sentence-transformers` reference embeddings for
//! known inputs, within a cosine-similarity tolerance (not bit-exact —
//! Candle/PyTorch numerical differences across platforms are expected).
//!
//! Network-touching on a cold `cache_dir` (first run only — `hf-hub` reuses
//! the cache afterward); gated behind an explicit opt-in env var so `cargo
//! test --workspace` stays fully offline by default. The reference fixture
//! itself (`fixtures/bge_base_en_v1_5_reference.json`, a small JSON file of
//! sentences + float vectors — not the model weights) was generated once,
//! offline, via `sentence-transformers`; loading *that* file never touches
//! the network, only constructing `BgeEmbedder` does.

use std::path::Path;

use attic_semantic::{
    BgeEmbedder, CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider, cosine,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct ReferenceFixture {
    model_revision: String,
    sentences: Vec<String>,
    vectors: Vec<Vec<f32>>,
}

/// Minimum acceptable cosine similarity between Candle's output and the
/// canonical `sentence-transformers` reference vector for the same input.
/// Not 1.0 (bit-exact) — different BLAS backends and floating-point
/// summation order between Candle and PyTorch legitimately produce tiny
/// numerical differences.
const MIN_COSINE_SIMILARITY: f32 = 0.999;

fn should_run() -> bool {
    std::env::var("ATTIC_TEST_BGE_NETWORK").as_deref() == Ok("1")
}

#[test]
fn bge_embedder_matches_sentence_transformers_reference() {
    if !should_run() {
        eprintln!(
            "skipping: set ATTIC_TEST_BGE_NETWORK=1 to run (downloads/reuses a cached ~130MB model)"
        );
        return;
    }

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/bge_base_en_v1_5_reference.json");
    let fixture_text = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", fixture_path.display()));
    let fixture: ReferenceFixture =
        serde_json::from_str(&fixture_text).expect("fixture should be valid JSON");

    let cache_dir = std::env::temp_dir().join("attic-bge-test-cache");
    let embedder = BgeEmbedder::new(&cache_dir, 8).expect("BgeEmbedder should construct");

    // The resolved revision may legitimately drift from the fixture's if the
    // upstream repo has since moved — that alone is not a test failure (the
    // pipeline's OUTPUT correctness is what's under test here), but it's
    // surfaced for visibility since it does affect what "reference" means.
    let descriptor = embedder.descriptor();
    if descriptor.model_revision != fixture.model_revision {
        eprintln!(
            "note: live-resolved revision ({}) differs from the fixture's ({}); \
             re-generate the fixture if BGE reference outputs are suspected to have changed",
            descriptor.model_revision, fixture.model_revision
        );
    }

    let inputs: Vec<EmbeddingInput> = fixture
        .sentences
        .iter()
        .enumerate()
        .map(|(i, text)| EmbeddingInput {
            unit_key: format!("ref-{i}"),
            text: text.clone(),
        })
        .collect();

    let cancel = CancelFlag::new();
    let mut usage = ResourceUsage::default();
    let outputs = embedder
        .embed_batch(&inputs, &cancel, &mut usage, None)
        .expect("reference embedding batch should succeed");

    assert_eq!(outputs.len(), fixture.vectors.len());
    for (i, (output, reference)) in outputs.iter().zip(&fixture.vectors).enumerate() {
        let sim = cosine(&output.vector, reference);
        assert!(
            sim >= MIN_COSINE_SIMILARITY,
            "sentence {i} ({:?}...): cosine similarity {sim} below threshold {MIN_COSINE_SIMILARITY}",
            fixture.sentences[i].chars().take(40).collect::<String>()
        );
    }
}
