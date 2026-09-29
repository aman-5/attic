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
        // Must stay typed across the IPC boundary. Flattening it into
        // `internal` erased the fact that it is recoverable, so the caller's
        // substring-based transient check missed it, burned a retry attempt
        // per occurrence, and permanently quarantined good units after three
        // — for a condition that clears on its own.
        S::DevicePressure(m) => WorkerFail {
            class: WorkerErrorClass::ResourcePressure,
            message: m,
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
            // All Candle devices share one construction path: the device is
            // chosen by the process-wide preference, which we set here from
            // the backend the server requested. This is the only place the
            // worker process learns which device to use — it never sees the
            // server's config object.
            //
            // A GPU request that cannot be honoured degrades to CPU inside
            // `Qwen3Embedder` and is reported back via the fingerprint; the
            // supervisor deliberately does not treat that as an identity
            // mismatch (see `verify_identity_capabilities`).
            "candle-cpu" | "candle-cuda" | "candle-metal" => {
                let pref = match spec.backend.as_str() {
                    "candle-cuda" => attic_semantic::DevicePreference::Cuda,
                    "candle-metal" => attic_semantic::DevicePreference::Metal,
                    _ => attic_semantic::DevicePreference::Cpu,
                };
                attic_semantic::device::set_process_preference(pref);

                let cache = std::path::PathBuf::from(&spec.cache_dir);
                let embedder = attic_semantic::Qwen3Embedder::new(
                    &cache,
                    spec.batch_size,
                    spec.dimension,
                    attic_semantic::QwenPooling::LastToken,
                )
                .map_err(map_semantic_error)?;
                if let Some(reason) = embedder.device_fallback_reason() {
                    tracing::warn!(
                        requested = %spec.backend,
                        actual = embedder.execution_backend().as_str(),
                        reason = %reason,
                        "requested GPU backend unavailable; running on CPU"
                    );
                }
                Arc::new(embedder)
            }
            #[cfg(all(windows, target_env = "msvc"))]
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
            #[cfg(not(all(windows, target_env = "msvc")))]
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
    // stdout is the protocol channel; diagnostics go to stderr, which the
    // supervisor forwards to the parent's stderr. Without a subscriber every
    // worker-side warning (DirectML pass failures, thermal pauses, VRAM
    // admission) was silently discarded.
    let filter = tracing_subscriber::EnvFilter::try_from_env("ATTIC_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
    let engine = NeuralEngine {
        provider: None,
        backend: "unloaded".to_string(),
    };
    attic_inference_protocol::engine::run_worker_loop(engine, std::io::stdin(), std::io::stdout())
}
