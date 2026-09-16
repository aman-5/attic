//! SemanticProvider adapter over the supervised inference worker (r06).
//!
//! Neural inference (Candle Qwen3, ORT/DirectML) runs in a child process so
//! a hung or crashed native runtime is killable and never wedges the MCP
//! server. This adapter preserves the `SemanticProvider` contract: typed
//! errors map from the worker's error classes, and a worker timeout kills
//! the child and surfaces as a typed failure the enrichment layer can retry
//! against a freshly restarted worker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use attic_inference_protocol::supervisor::{
    LoadParams, SupervisorError, WorkerLaunch, WorkerSupervisor,
};
use attic_inference_protocol::{EmbedItem, WorkerErrorClass};

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ProviderConcurrencyContract,
    ResourceUsage, SemanticProvider,
};

/// How long a single embedding batch may run inside the worker before the
/// supervisor kills it. Generous for cold starts; the resource governor
/// (r08) tightens effective behavior via batch size, not this bound.
const EMBED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

pub struct SupervisedWorkerProvider {
    supervisor: WorkerSupervisor,
    fingerprint: EmbeddingFingerprint,
    max_input_bytes: usize,
    ready: AtomicBool,
}

impl SupervisedWorkerProvider {
    /// Create the provider; spawns nothing yet (lazy start on first embed so
    /// server startup never blocks on the worker or model load).
    pub fn new(
        launch: WorkerLaunch,
        load: LoadParams,
        fingerprint: EmbeddingFingerprint,
        max_input_bytes: usize,
    ) -> Self {
        let supervisor = WorkerSupervisor::new(launch);
        // Remember load params immediately so lazy restart works.
        let _ = supervisor.load_model_params_only(load);
        Self {
            supervisor,
            fingerprint,
            max_input_bytes,
            ready: AtomicBool::new(false),
        }
    }

    fn ensure_ready(&self) -> Result<(), SemanticError> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        self.supervisor.handshake().map_err(map_supervisor_error)?;
        self.supervisor
            .load_model_remembered()
            .map_err(map_supervisor_error)?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }
}

fn map_supervisor_error(e: SupervisorError) -> SemanticError {
    match e {
        SupervisorError::WorkerTimeout(d) => SemanticError::EmbeddingFailed(format!(
            "inference worker exceeded {d:?} deadline and was killed; it will restart on the next batch"
        )),
        SupervisorError::WorkerDied => SemanticError::EmbeddingFailed(
            "inference worker died mid-batch; it will restart on the next batch".into(),
        ),
        SupervisorError::Engine { class, message } => match class {
            WorkerErrorClass::OutOfMemory => SemanticError::BudgetExhausted(message),
            WorkerErrorClass::InvalidInput => SemanticError::EmbeddingFailed(message),
            WorkerErrorClass::Artifact => SemanticError::ProviderUnavailable {
                provider: "inference-worker".into(),
                reason: message,
            },
            _ => SemanticError::EmbeddingFailed(message),
        },
        other => SemanticError::ProviderUnavailable {
            provider: "inference-worker".into(),
            reason: other.to_string(),
        },
    }
}

impl SemanticProvider for SupervisedWorkerProvider {
    fn id(&self) -> &'static str {
        "qwen3-supervised"
    }

    fn model_id(&self) -> &str {
        "qwen3-embedding-0.6b"
    }

    fn dimensions(&self) -> usize {
        self.fingerprint.dimension
    }

    fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    fn available(&self) -> bool {
        // The worker is lazily spawned; availability means "configured".
        true
    }

    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        // One worker process owns the model/device session.
        ProviderConcurrencyContract::Serialized
    }

    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        Some(self.fingerprint.clone())
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        if cancel.is_cancelled() {
            return Err(SemanticError::Cancelled {
                completed: 0,
                total: inputs.len(),
            });
        }
        self.ensure_ready()?;

        let items: Vec<EmbedItem> = inputs
            .iter()
            .map(|i| EmbedItem {
                key: i.unit_key.clone(),
                text: i.text.clone(),
            })
            .collect();
        let keys: Vec<String> = inputs.iter().map(|i| i.unit_key.clone()).collect();

        let effective_deadline = match deadline {
            Some(d) => {
                let remaining = d.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(SemanticError::Cancelled {
                        completed: 0,
                        total: inputs.len(),
                    });
                }
                remaining.min(EMBED_DEADLINE)
            }
            None => EMBED_DEADLINE,
        };

        let vectors = self
            .supervisor
            .embed_batch(items, effective_deadline)
            .map_err(map_supervisor_error)?;

        if vectors.len() != keys.len() {
            return Err(SemanticError::EmbeddingFailed(format!(
                "worker returned {} vectors for {} inputs",
                vectors.len(),
                keys.len()
            )));
        }

        usage.input_bytes += inputs.iter().map(|i| i.text.len() as u64).sum::<u64>();
        usage.items_embedded += keys.len() as u64;
        Ok(keys
            .into_iter()
            .zip(vectors)
            .map(|(unit_key, vector)| EmbeddingOutput { unit_key, vector })
            .collect())
    }
}

/// The fingerprint the worker's provider WILL have once loaded — needed
/// before the child exists so enrichment can key generations. Must match
/// the in-worker providers exactly (qwen3_provider / ort_directml); the
/// real provider reports its fingerprint after load and a mismatch would
/// produce a new vector space rather than silent mixing.
pub fn expected_fingerprint(backend: &str, dimension: Option<usize>) -> EmbeddingFingerprint {
    use crate::provider::ExecutionBackend;
    let base = EmbeddingFingerprint {
        provider: "qwen3".into(),
        model_id: crate::qwen3_provider::QWEN_MODEL_ID.into(),
        model_revision: String::new(),
        dimension: dimension.unwrap_or(1024),
        pooling_version: "last_token_v1".into(),
        normalization_version: "l2_unit_v1".into(),
        tokenizer_version: "qwen_bpe_v1".into(),
        chunking_version: attic_core::constants::CHUNKING_VERSION.into(),
        query_instruction_version: crate::instruction::CODE_RETRIEVAL_V1_ID.into(),
        execution_backend: ExecutionBackend::Unknown,
        quantization: String::new(),
    };
    match backend {
        "ort-directml" => EmbeddingFingerprint {
            model_revision: "onnx-community-fp16".into(),
            execution_backend: ExecutionBackend::OrtDirectMl,
            quantization: "fp16-onnx".into(),
            ..base
        },
        _ => EmbeddingFingerprint {
            model_revision: crate::model_assets::ModelManifest::qwen3_default().pinned_revision,
            execution_backend: ExecutionBackend::CandleCpu,
            quantization: "fp32-safetensors".into(),
            ..base
        },
    }
}

/// The max_input_bytes the worker's provider will enforce, without loading
/// the model (mirrors each provider's contract).
pub fn expected_max_input_bytes(backend: &str, seq_len: usize) -> usize {
    match backend {
        // DirectML: fixed sequence length x conservative bytes-per-token.
        "ort-directml" => seq_len * 2,
        // Candle: DEFAULT_MAX_TOKENS (1024) x MIN_BYTES_PER_TOKEN (2).
        _ => 1024 * 2,
    }
}
