//! `attic inference-worker` — supervised child-process entry point (r06).
//!
//! The supervisor (parent `attic` process) speaks the length-prefixed
//! protocol from `attic-inference-protocol` over this process's stdin/stdout.
//! This module adapts the real neural providers (Candle Qwen3 CPU, ORT
//! DirectML GPU) to the engine contract. stdout is the protocol channel —
//! NEVER log to it; diagnostics go to stderr only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use attic_inference_protocol::engine::{EngineInfo, LoadSpec, WorkerEngine, WorkerFail};
use attic_inference_protocol::{EmbedItem, WorkerErrorClass};
use attic_semantic::provider::SemanticProvider;
use attic_semantic::{CancelFlag, EmbeddingInput, ResourceUsage};

struct NeuralEngine {
    provider: Option<Arc<dyn SemanticProvider>>,
    backend: String,
}

impl NeuralEngine {
    fn provider(&self) -> Result<&Arc<dyn SemanticProvider>, WorkerFail> {
        self.provider
            .as_ref()
            .ok_or_else(|| WorkerFail::artifact("model not loaded"))
    }
}

fn map_semantic_error(e: attic_semantic::SemanticError) -> WorkerFail {
    use attic_semantic::SemanticError as S;
    match e {
        S::InputTooManyTokens { tokens, max } => WorkerFail {
            class: WorkerErrorClass::InvalidInput,
            message: format!("input exceeds provider token budget ({tokens} > {max})"),
        },
        S::BudgetExhausted(m) => WorkerFail {
            class: WorkerErrorClass::OutOfMemory,
            message: m,
        },
        S::ProviderUnavailable { reason, .. } => WorkerFail {
            class: WorkerErrorClass::Artifact,
            message: reason,
        },
        other => WorkerFail::internal(other.to_string()),
    }
}

impl WorkerEngine for NeuralEngine {
    fn backend_name(&self) -> &str {
        &self.backend
    }

    fn load(&mut self, spec: &LoadSpec) -> Result<EngineInfo, WorkerFail> {
        let provider: Arc<dyn SemanticProvider> = match spec.backend.as_str() {
            "candle-cpu" => {
                let cache = std::path::PathBuf::from(&spec.cache_dir);
                let embedder = attic_semantic::Qwen3Embedder::new(
                    &cache,
                    spec.batch_size,
                    spec.dimension,
                    attic_semantic::QwenPooling::LastToken,
                )
                .map_err(map_semantic_error)?;
                Arc::new(embedder)
            }
            #[cfg(feature = "ort-directml")]
            "ort-directml" => {
                let dir = spec
                    .onnx_model_dir
                    .as_ref()
                    .ok_or_else(|| WorkerFail::artifact("ort-directml requires onnx_model_dir"))?;
                let p = attic_semantic::OrtDirectMlProvider::from_model_dir(
                    std::path::Path::new(dir),
                    spec.batch_size,
                    spec.seq_len.unwrap_or(512),
                    spec.dimension,
                )
                .map_err(map_semantic_error)?;
                Arc::new(p)
            }
            #[cfg(not(feature = "ort-directml"))]
            "ort-directml" => {
                return Err(WorkerFail::artifact(
                    "ort-directml backend not compiled into this binary",
                ));
            }
            other => {
                return Err(WorkerFail::artifact(format!(
                    "unknown inference backend '{other}'"
                )));
            }
        };

        let fp = provider.fingerprint();
        self.backend = spec.backend.clone();
        self.provider = Some(provider);
        let p = self.provider()?;
        let mut capabilities = vec!["qwen3-embedding".into()];
        if let Some(fp) = fp.as_ref() {
            // Carry the ACTUAL loaded identity back to the supervisor so it
            // can be verified against the identity assumed before this
            // worker existed, rather than trusting that load did what was
            // asked.
            capabilities.extend(attic_semantic::fingerprint_capabilities(fp));
        }
        Ok(EngineInfo {
            backend: self.backend.clone(),
            capabilities,
            dimension: fp.as_ref().map(|f| f.dimension).unwrap_or(0),
            max_input_bytes: p.max_input_bytes(),
        })
    }

    fn embed(
        &mut self,
        items: &[EmbedItem],
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, WorkerFail> {
        let provider = self.provider()?.clone();
        let inputs: Vec<EmbeddingInput> = items
            .iter()
            .map(|i| EmbeddingInput {
                unit_key: i.key.clone(),
                text: i.text.clone(),
            })
            .collect();
        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        let dl = Instant::now() + deadline;
        let outputs = provider
            .embed_batch(&inputs, &cancel, &mut usage, Some(dl))
            .map_err(map_semantic_error)?;
        Ok(outputs.into_iter().map(|o| o.vector).collect())
    }
}

/// Run the worker loop on stdin/stdout. Called from `main()` before any
/// tokio runtime exists; exits the process when the supervisor closes the
/// channel or sends Shutdown.
pub fn run_inference_worker() -> i32 {
    let engine = NeuralEngine {
        provider: None,
        backend: "unloaded".to_string(),
    };
    attic_inference_protocol::engine::run_worker_loop(engine, std::io::stdin(), std::io::stdout())
}
