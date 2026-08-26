//! Provider-neutral embedding contract (Phase 5 §6, ADR-013).
//!
//! Attic's retrieval/storage/MCP architecture depends ONLY on this trait —
//! never on a vendor SDK, model format, or runtime. A future provider
//! (e.g. an ONNX `fastembed` backend) is a drop-in implementation.
//!
//! Contract invariants:
//! * `embed_batch` receives PRE-REDACTED text only (Phase 1B + §18 defense).
//! * Implementations must be deterministic for (model, input) pairs.
//! * Cancellation is cooperative: checked between items/batches; completed
//!   items may still be returned alongside the error.
//! * Resource accounting is observable, never hidden inside the provider.

use crate::error::SemanticError;

/// Cooperative cancellation flag shared between coordinator and provider.
#[derive(Debug, Default)]
pub struct CancelFlag(pub std::sync::atomic::AtomicBool);

impl CancelFlag {
    pub fn new() -> Self {
        Self(std::sync::atomic::AtomicBool::new(false))
    }
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One text awaiting an embedding.
#[derive(Debug, Clone)]
pub struct EmbeddingInput {
    /// Stable semantic-unit identity string (lineage, §5).
    pub unit_key: String,
    /// Pre-redacted text to embed. Never raw secret-bearing content.
    pub text: String,
}

/// One produced embedding.
#[derive(Debug, Clone)]
pub struct EmbeddingOutput {
    pub unit_key: String,
    /// L2-normalized vector (providers MUST normalize so cosine == dot).
    pub vector: Vec<f32>,
}

/// Observable resource consumption of provider work.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResourceUsage {
    pub items_embedded: u64,
    pub input_bytes: u64,
    pub elapsed_ms: u64,
}

impl ResourceUsage {
    pub fn merge(&mut self, o: &Self) {
        self.items_embedded += o.items_embedded;
        self.input_bytes += o.input_bytes;
        self.elapsed_ms += o.elapsed_ms;
    }
}

/// Provider-neutral embedding contract (ADR-013). Object-safe so any
/// backend can be installed without changing callers.
pub trait SemanticProvider: Send + Sync {
    /// Stable provider id (e.g. "hashing", "fastembed").
    fn id(&self) -> &'static str;

    /// Stable model/version identity embedded into every record's lineage.
    fn model_id(&self) -> &str;

    /// Fixed output dimensionality.
    fn dimensions(&self) -> usize;

    /// Maximum accepted input size per item, in bytes.
    fn max_input_bytes(&self) -> usize;

    /// Cheap availability probe (model files present, endpoint reachable…).
    fn available(&self) -> bool {
        true
    }

    /// Embed a batch. Returns outputs for the items it managed to embed;
    /// on cancellation/partial failure it returns `Cancelled`/`EmbeddingFailed`
    /// with whatever completed so far attached via `completed`.
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError>;
}

/// Convenience: cosine similarity for L2-normalized vectors (dot product),
/// with a length guard against malformed records.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter()
        .zip(b)
        .map(|(x, y)| x * y)
        .sum::<f32>()
        .clamp(-1.0, 1.0)
}
