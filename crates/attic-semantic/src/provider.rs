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

use serde::{Deserialize, Serialize};

use crate::embedding_profile::EmbeddingSpaceDescriptor;
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

    /// The resolved, immutable vector-space identity this provider actually
    /// produces (Low-Level Design §3) — `None` for providers with no
    /// persisted-identity concept (e.g. `HashingEmbedder`, test doubles).
    /// Only a `Some` return causes the enrichment worker to claim/compare an
    /// `EmbeddingProfile` before real embedding work; a provider that never
    /// overrides this default never participates in profile claiming at all.
    fn embedding_descriptor(&self) -> Option<EmbeddingSpaceDescriptor> {
        None
    }

    /// Embed a batch under an ENFORCEABLE time contract (§20): `deadline`
    /// (when set) bounds the whole call — implementations must check it
    /// cooperatively between items/slices and return
    /// [`SemanticError::Cancelled`] with nothing committed when it fires, so
    /// callers can degrade inside their configured semantic budget instead
    /// of blocking indefinitely. A provider that ignores the deadline is in
    /// violation of this contract and fails the conformance tests.
    ///
    /// Returns outputs for the items it managed to embed; on
    /// cancellation/partial failure it returns `Cancelled`/`EmbeddingFailed`
    /// with whatever completed so far attached via `completed`.
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<std::time::Instant>,
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

/// Comprehensive architectural fingerprint of an active embedding vector space (Final Master Plan V2 §51).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingFingerprint {
    /// Identifier of the provider (e.g. "bge", "qwen3", "hashing").
    pub provider: String,
    /// Identifier of the neural model (e.g. "bge-base-en-v1.5", "qwen3-embedding-0.6b").
    pub model_id: String,
    /// Exact pinned git revision or weights SHA.
    pub model_revision: String,
    /// Output vector dimensionality.
    pub dimension: usize,
    /// Pooling algorithm and version (e.g. "cls_v1", "last_token_v1", "mean_v1").
    pub pooling_version: String,
    /// Normalization strategy (e.g. "l2_unit_v1").
    pub normalization_version: String,
    /// Tokenizer vocabulary/code version.
    pub tokenizer_version: String,
    /// Chunking/windowing strategy version.
    pub chunking_version: String,
    /// Query instruction template version (e.g. "code_retrieval_v1").
    pub query_instruction_version: String,
}

/// Resource limits allocated to an embedding inference call (Final Master Plan V2 §27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingExecutionBudget {
    /// Dedicated CPU threads allocated for this inference pass.
    pub cpu_threads: usize,
    /// Maximum number of items in a single forward pass.
    pub max_batch_size: usize,
    /// Optional hard deadline for cooperative cancellation.
    pub deadline: Option<std::time::Instant>,
}

impl Default for EmbeddingExecutionBudget {
    fn default() -> Self {
        Self {
            cpu_threads: 2,
            max_batch_size: 16,
            deadline: None,
        }
    }
}

/// Provider thread-safety and concurrency contract (Master Plan V2 §20).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderConcurrencyContract {
    /// A single instance safely supports concurrent calls from multiple threads.
    SharedConcurrent,
    /// Calls to a single instance are serialized internally (e.g. via mutex).
    Serialized,
    /// Concurrent callers require a bounded pool of lane instances.
    PooledLanes { max_lanes: usize },
}

/// Master embedding provider contract (Final Master Plan V2 §27).
pub trait EmbeddingProvider: Send + Sync {
    /// Return the immutable architectural fingerprint of the vector space.
    fn model_fingerprint(&self) -> EmbeddingFingerprint;

    /// Fixed dimensionality of vectors produced by this provider.
    fn dimension(&self) -> usize;

    /// Explicitly declares the concurrency contract of this provider (§20).
    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        ProviderConcurrencyContract::Serialized
    }

    /// Warm up model tensors and runtime resources before high-throughput batching.
    fn warm_up(&self, budget: &EmbeddingExecutionBudget) -> Result<(), SemanticError>;

    /// Embed a batch of documents under the given execution budget.
    fn embed_documents(
        &self,
        inputs: &[EmbeddingInput],
        budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError>;

    /// Embed a single query string for interactive semantic search.
    fn embed_query(
        &self,
        query: &str,
        budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<f32>, SemanticError>;
}

