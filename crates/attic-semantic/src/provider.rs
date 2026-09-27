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
use std::time::Instant;

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

    /// Model lifecycle state for status reporting, when the provider has one
    /// (e.g. `DeferredProvider` during background download). `None` for
    /// providers that are statically ready or unavailable.
    fn model_lifecycle(&self) -> Option<String> {
        None
    }

    /// Declare how many callers may execute inference against this provider.
    ///
    /// Queue workers must honor this contract before claiming work. The
    /// default preserves concurrency for lightweight/test providers; neural
    /// providers that protect one model behind a mutex must report
    /// `Serialized` so waiting callers do not hoard queue items or divide the
    /// CPU budget into lanes that cannot actually run concurrently.
    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        ProviderConcurrencyContract::SharedConcurrent
    }

    /// Return the immutable architectural fingerprint of the vector space, if known.
    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        None
    }

    /// Truthful fallback-state explanation, for providers that wrap more
    /// than one backend and may have switched away from their primary
    /// (e.g. GPU→CPU escalation after a permanent GPU failure). `None`
    /// means "never fell back" — never fabricated, and cleared again once
    /// a provider is back on its primary backend.
    fn fallback_reason(&self) -> Option<String> {
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

/// Execution backend that produced a vector — TELEMETRY ONLY (Final Master
/// Plan identity split). Two backends may write to the same vector space only
/// after measured parity; the backend itself never participates in identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionBackend {
    CandleCpu,
    CandleCuda,
    CandleMetal,
    OrtDirectMl,
    OrtCoreMl,
    Hashing,
    #[default]
    Unknown,
}

impl ExecutionBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CandleCpu => "candle-cpu",
            Self::CandleCuda => "candle-cuda",
            Self::CandleMetal => "candle-metal",
            Self::OrtDirectMl => "ort-directml",
            Self::OrtCoreMl => "ort-coreml",
            Self::Hashing => "hashing",
            Self::Unknown => "unknown",
        }
    }
}

/// Comprehensive architectural fingerprint of an active embedding vector space (Final Master Plan V2 §51).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingFingerprint {
    /// Identifier of the provider (e.g. "qwen3", "hashing").
    pub provider: String,
    /// Identifier of the neural model (e.g. "qwen3-embedding-0.6b", "hashed-ngram-v1").
    pub model_id: String,
    /// Exact pinned git revision or weights SHA.
    pub model_revision: String,
    /// Output vector dimensionality.
    pub dimension: usize,
    /// Pooling algorithm and version (e.g. "last_token_v1", "mean_v1").
    pub pooling_version: String,
    /// Normalization strategy (e.g. "l2_unit_v1").
    pub normalization_version: String,
    /// Tokenizer vocabulary/code version.
    pub tokenizer_version: String,
    /// Chunking/windowing strategy version. NOT part of the vector-space
    /// identity — chunking changes which texts exist, not the space they
    /// live in. Carried here for lineage; see [`Self::vector_space_id`].
    pub chunking_version: String,
    /// Query instruction template version (e.g. "code_retrieval_v1").
    pub query_instruction_version: String,
    /// Execution backend that produced vectors (telemetry only — excluded
    /// from both identity hashes below; serde default keeps pre-split
    /// rows readable).
    #[serde(default)]
    pub execution_backend: ExecutionBackend,
    /// Weight quantization of the model artifact (e.g. "q8_0", "fp16",
    /// "fp32"). PART of vector-space identity: quantized and full-precision
    /// weights produce measurably different vectors, so mixing them in one
    /// space would corrupt similarity. Serde default keeps pre-r04 rows
    /// readable ("unknown" never matches a real configured value).
    #[serde(default = "default_quantization_unknown")]
    pub quantization: String,
}

fn default_quantization_unknown() -> String {
    "unknown".to_string()
}

impl EmbeddingFingerprint {
    /// Vector-space identity: model artifact, quantization, tokenizer,
    /// pooling, normalization, dimension, instruction — the things that
    /// determine whether two vectors may be compared at all. Chunking and
    /// backend are deliberately excluded (chunking selects texts; backend is
    /// telemetry).
    pub fn vector_space_id(&self) -> String {
        let canonical = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.provider,
            self.model_id,
            self.model_revision,
            self.quantization,
            self.dimension,
            self.pooling_version,
            self.normalization_version,
            self.tokenizer_version,
            self.query_instruction_version,
        );
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }

    /// Content-generation identity: which texts were selected and how they
    /// were produced. Changing this invalidates the content, not the space.
    pub fn content_generation_id(&self, selection_version: &str) -> String {
        let canonical = format!(
            "{}|{}|{}",
            self.chunking_version,
            selection_version,
            attic_core::constants::ANALYZER_REGISTRY_VERSION,
        );
        blake3::hash(canonical.as_bytes()).to_hex().to_string()
    }
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

impl ProviderConcurrencyContract {
    /// Clamp a requested queue-worker count to the provider's runnable
    /// inference concurrency.
    pub fn effective_workers(self, requested: usize) -> usize {
        let requested = requested.max(1);
        match self {
            Self::Serialized => 1,
            Self::SharedConcurrent => requested,
            Self::PooledLanes { max_lanes } => requested.min(max_lanes.max(1)),
        }
    }
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

/// Fallback provider that reports unavailable when models are missing or disabled.
#[derive(Debug, Default)]
pub struct UnavailableProvider {
    pub reason: String,
}

impl SemanticProvider for UnavailableProvider {
    fn id(&self) -> &'static str {
        "unavailable"
    }
    fn model_id(&self) -> &str {
        "none-v0"
    }
    fn dimensions(&self) -> usize {
        8
    }
    fn max_input_bytes(&self) -> usize {
        1024
    }
    fn available(&self) -> bool {
        false
    }
    fn embed_batch(
        &self,
        _: &[EmbeddingInput],
        _: &CancelFlag,
        _: &mut ResourceUsage,
        _: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        Err(SemanticError::ProviderUnavailable {
            provider: "unavailable".into(),
            reason: self.reason.clone(),
        })
    }
}

#[cfg(test)]
mod identity_split_tests {
    use super::*;

    fn fp(model_rev: &str) -> EmbeddingFingerprint {
        EmbeddingFingerprint {
            provider: "qwen3".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: model_rev.into(),
            dimension: 1024,
            pooling_version: "last_token_v1".into(),
            normalization_version: "l2_unit_v1".into(),
            tokenizer_version: "qwen_bpe_v1".into(),
            chunking_version: attic_core::constants::CHUNKING_VERSION.into(),
            query_instruction_version: "code_retrieval_v1".into(),
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "test-none".to_string(),
        }
    }

    #[test]
    fn vector_space_ignores_chunking_and_backend() {
        let a = fp("rev1");
        let mut b = fp("rev1");
        b.chunking_version = "json_router_v2".into();
        b.execution_backend = ExecutionBackend::OrtDirectMl;
        assert_eq!(
            a.vector_space_id(),
            b.vector_space_id(),
            "chunking/backend must not alter vector-space identity"
        );
    }

    #[test]
    fn vector_space_changes_with_model_revision() {
        assert_ne!(fp("rev1").vector_space_id(), fp("rev2").vector_space_id());
    }

    #[test]
    fn content_generation_changes_with_chunking() {
        let a = fp("rev1");
        let mut b = fp("rev1");
        b.chunking_version = "json_router_v2".into();
        assert_ne!(
            a.content_generation_id("sel_v1"),
            b.content_generation_id("sel_v1"),
            "chunking change must produce a new content generation"
        );
    }

    #[test]
    fn content_generation_ignores_backend() {
        let a = fp("rev1");
        let mut b = fp("rev1");
        b.execution_backend = ExecutionBackend::OrtDirectMl;
        assert_eq!(
            a.content_generation_id("sel_v1"),
            b.content_generation_id("sel_v1")
        );
    }

    #[test]
    fn pre_split_rows_deserialize_with_unknown_backend() {
        let legacy = r#"{"provider":"qwen3","model_id":"m","model_revision":"r","dimension":1024,"pooling_version":"p","normalization_version":"n","tokenizer_version":"t","chunking_version":"ast_v1","query_instruction_version":"q"}"#;
        let parsed: EmbeddingFingerprint = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.execution_backend, ExecutionBackend::Unknown);
    }
}
