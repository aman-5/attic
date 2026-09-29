//! ONNX Runtime + DirectML provider for Qwen3-Embedding-0.6B (Phase 4).
//!
//! Validated by the Phase 0B spike on an RTX A500 (4 GB VRAM):
//! - parity vs HF transformers reference: min cosine 0.99993 (fp16)
//! - throughput: 3,130 tok/s at seq budget 512, batch 8 (vs ~16 tok/s CPU)
//! - no CUDA toolkit, no admin rights required; runs on NVIDIA/AMD/Intel GPUs
//!
//! Hard-won constraints baked in here:
//! - DirectML requires STATIC shapes: dynamic Reshape crashes at runtime, so
//!   the session is built with `sequence_length` overridden to a fixed token
//!   budget and every input is padded/truncated to it.
//! - The onnx-community export is decoder-with-past: 28 layers × key/value
//!   `past_key_values.*` inputs must be fed as EMPTY f16 tensors for
//!   single-pass embedding.
//! - DirectML sessions are NOT thread-safe for concurrent `Run()` (documented
//!   crash, onnxruntime issue #22147): this provider declares
//!   `ProviderConcurrencyContract::Serialized` and guards the session with a
//!   mutex — never parallelize calls into one session.
//! - Token budget is decisive (2.3× measured): inputs are sorted by length
//!   and sub-batched so `batch_items × padded_seq_len` stays under budget.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use half::f16;
use ndarray::{Array2, Array4};
use ort::ep::DirectML;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer};

use attic_storage::gpu_telemetry::{AdmissionDecision, GpuAdmissionController, GpuTelemetry};

use crate::error::SemanticError;
use crate::instruction::CODE_RETRIEVAL_V1_ID;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ExecutionBackend,
    ProviderConcurrencyContract, ResourceUsage, SemanticProvider,
};

/// Fraction of a device's dedicated VRAM this process will admit against,
/// leaving the remainder for the OS/compositor/other apps. A desktop GPU is
/// shared hardware; assuming we own it is how admission ends up thrashing.
const VRAM_CEILING_FRACTION_PCT: u64 = 75;

/// Fallback ceiling when the device reports no usable VRAM telemetry —
/// deliberately below the smallest GPU this provider has been validated on
/// (RTX A500, 4096 MiB). Override with `ATTIC_VRAM_CEILING_MIB`.
const DEFAULT_VRAM_CEILING_MIB: u64 = 3072;

/// Ceiling admission enforces against.
///
/// Previously a flat 3072 MiB regardless of hardware, which silently
/// under-used large cards and mis-sized small ones. Derive it from what the
/// device actually reports, and only fall back to the constant when
/// telemetry is unavailable.
fn default_vram_ceiling_mib() -> u64 {
    match attic_storage::gpu_telemetry::query_vram_snapshot().total_mib {
        Some(total) if total > 0 => (total.saturating_mul(VRAM_CEILING_FRACTION_PCT) / 100).max(1),
        _ => DEFAULT_VRAM_CEILING_MIB,
    }
}

fn vram_telemetry() -> &'static GpuTelemetry {
    static TELEMETRY: OnceLock<GpuTelemetry> = OnceLock::new();
    TELEMETRY.get_or_init(GpuTelemetry::new)
}

fn vram_admission() -> &'static GpuAdmissionController {
    static ADMISSION: OnceLock<GpuAdmissionController> = OnceLock::new();
    ADMISSION.get_or_init(|| {
        let ceiling = std::env::var("ATTIC_VRAM_CEILING_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(default_vram_ceiling_mib);
        GpuAdmissionController::new(ceiling)
    })
}

/// Provider id for the ORT/DirectML path.
pub const ORT_PROVIDER_ID: &str = "qwen3-ort";

/// Qwen3-Embedding-0.6B has 28 transformer layers, 8 KV heads, head_dim 128.
const NUM_LAYERS: usize = 28;
const NUM_KV_HEADS: usize = 8;
const HEAD_DIM: usize = 128;
/// Model's native output width before Matryoshka truncation.
const NATIVE_DIMS: usize = 1024;
/// Floor bytes-per-token for conservative pre-tokenization estimates.
const MIN_BYTES_PER_TOKEN: usize = 2;

/// Empty-KV element type, detected from the model's own input signature
/// (fp16 export uses f16; the Q8/int8 exports use f32). Never assume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KvDType {
    F16,
    F32,
}

/// ONNX Runtime DirectML provider for Qwen3-Embedding-0.6B (fp16).
pub struct OrtDirectMlProvider {
    /// DirectML sessions forbid concurrent Run() — this mutex is the
    /// enforcement of the Serialized contract, not an afterthought.
    session: Mutex<Session>,
    tokenizer: Tokenizer,
    /// Fixed padded sequence length (DirectML static-shape requirement).
    seq_len: usize,
    /// Max items per forward pass (item-count cap; token budget dominates).
    batch_size: usize,
    /// `batch_items × seq_len` padded-token budget per forward pass.
    batch_token_budget: usize,
    target_dims: usize,
    fingerprint: EmbeddingFingerprint,
    kv_dtype: KvDType,
}

impl OrtDirectMlProvider {
    /// Build a provider from a local ONNX model directory containing
    /// `model_fp16.onnx` (+ its `.onnx_data`) and `tokenizer.json`.
    ///
    /// `seq_len` is the fixed padded sequence length — the spike measured
    /// 512 as dramatically better than 1024 (padding waste dominates), so
    /// prefer 512 unless corpus chunks are consistently longer.
    pub fn from_model_dir(
        model_dir: &Path,
        batch_size: usize,
        seq_len: usize,
        dimension_override: Option<usize>,
    ) -> Result<Self, SemanticError> {
        let model_path = model_dir.join("model_fp16.onnx");
        let tokenizer_path = model_dir.join("tokenizer.json");
        if !model_path.is_file() {
            return Err(SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("ONNX model not found at {}", model_path.display()),
            });
        }

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("failed to load tokenizer: {e}"),
            }
        })?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::Fixed(seq_len),
            ..Default::default()
        }));
        // NO truncation is configured here on purpose (r04): silently
        // clipping an over-budget input would embed only its head and store
        // a vector claiming to represent the whole unit. run_forward rejects
        // over-token inputs with InputTooManyTokens instead.

        let session = Session::builder()
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("session builder: {e}"),
            })?
            .with_execution_providers([DirectML::default()
                .with_performance_preference(
                    ort::ep::directml::PerformancePreference::HighPerformance,
                )
                .build()])
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("execution providers: {e}"),
            })?
            .with_dimension_override("sequence_length", seq_len as i64)
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("dimension override: {e}"),
            })?
            .commit_from_file(&model_path)
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("failed to initialize DirectML session: {e}"),
            })?;

        let target_dims = dimension_override.unwrap_or(NATIVE_DIMS);
        if target_dims > NATIVE_DIMS {
            return Err(SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("dimension override {target_dims} exceeds native {NATIVE_DIMS}"),
            });
        }

        // Detect the KV-cache element type from the model's own input
        // signature — the fp16 export feeds f16 KV tensors, the Q8/int8
        // exports expect f32. Hardcoding either breaks the other.
        let kv_dtype = session
            .inputs()
            .iter()
            .find(|i| i.name() == "past_key_values.0.key")
            .map(|i| match i.dtype() {
                ort::value::ValueType::Tensor {
                    ty: ort::value::TensorElementType::Float32,
                    ..
                } => KvDType::F32,
                _ => KvDType::F16,
            })
            .unwrap_or(KvDType::F16);
        tracing::info!(?kv_dtype, "DirectML model KV-cache dtype detected");

        let fingerprint = EmbeddingFingerprint {
            provider: ORT_PROVIDER_ID.into(),
            model_id: crate::qwen3_provider::QWEN_MODEL_ID.into(),
            model_revision: "onnx-community-fp16".into(),
            dimension: target_dims,
            pooling_version: "last_token_v1".into(),
            normalization_version: "l2_unit_v1".into(),
            tokenizer_version: "qwen_bpe_v1".into(),
            chunking_version: attic_core::constants::CHUNKING_VERSION.into(),
            query_instruction_version: CODE_RETRIEVAL_V1_ID.into(),
            execution_backend: ExecutionBackend::OrtDirectMl,
            // onnx-community fp16 export — NOT Q8. Part of vector-space
            // identity (r04): fp16 vectors must never mix with Q8/fp32.
            quantization: "fp16-onnx".into(),
        };

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            seq_len,
            batch_size: batch_size.max(1),
            batch_token_budget: batch_size.max(1) * seq_len,
            target_dims,
            fingerprint,
            kv_dtype,
        })
    }

    /// Embed one batch of ≤ batch_size texts, each padded to seq_len.
    /// Caller holds the session mutex.
    ///
    /// Returns (pooled vectors at NATIVE width, per-item real token counts,
    /// native hidden width) — batch row slicing MUST use the native width,
    /// never the configured target dimension (r04 stride fix).
    fn run_forward(
        session: &mut Session,
        tokenizer: &Tokenizer,
        seq_len: usize,
        kv_dtype: KvDType,
        texts: &[&str],
    ) -> Result<(Vec<f32>, Vec<usize>, usize), SemanticError> {
        let batch = texts.len();
        let mut ids_flat: Vec<i64> = Vec::with_capacity(batch * seq_len);
        let mut mask_flat: Vec<i64> = Vec::with_capacity(batch * seq_len);
        let mut real_tokens: Vec<usize> = Vec::with_capacity(batch);
        for text in texts {
            let enc = tokenizer
                .encode(*text, true)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("tokenize: {e}")))?;
            let ids = enc.get_ids();
            if ids.len() > seq_len {
                return Err(SemanticError::InputTooManyTokens {
                    tokens: ids.len(),
                    max: seq_len,
                });
            }
            ids_flat.extend(ids.iter().map(|&i| i as i64));
            mask_flat.extend(enc.get_attention_mask().iter().map(|&m| m as i64));
            real_tokens.push(enc.get_attention_mask().iter().filter(|&&m| m == 1).count());
        }
        let pos_flat: Vec<i64> = (0..batch as i64).flat_map(|_| 0..seq_len as i64).collect();

        let to_tensor = |v: Vec<i64>| -> Result<Tensor<i64>, SemanticError> {
            let arr = Array2::from_shape_vec((batch, seq_len), v)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor shape: {e}")))?;
            Tensor::from_array(arr)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor: {e}")))
        };

        let mut inputs: Vec<(String, ort::value::DynValue)> = vec![
            ("input_ids".into(), to_tensor(ids_flat)?.into_dyn()),
            (
                "attention_mask".into(),
                to_tensor(mask_flat.clone())?.into_dyn(),
            ),
            ("position_ids".into(), to_tensor(pos_flat)?.into_dyn()),
        ];
        for layer in 0..NUM_LAYERS {
            for kv in ["key", "value"] {
                // Build the empty KV tensor in the dtype the MODEL declares
                // (fp16 export: f16; Q8/int8 exports: f32). Both arms yield
                // DynValue directly so the match types unify.
                let t: ort::value::DynValue = match kv_dtype {
                    KvDType::F16 => {
                        Tensor::from_array(Array4::<f16>::zeros((batch, NUM_KV_HEADS, 0, HEAD_DIM)))
                            .map_err(|e| SemanticError::EmbeddingFailed(format!("kv tensor: {e}")))?
                            .into_dyn()
                    }
                    KvDType::F32 => {
                        Tensor::from_array(Array4::<f32>::zeros((batch, NUM_KV_HEADS, 0, HEAD_DIM)))
                            .map_err(|e| SemanticError::EmbeddingFailed(format!("kv tensor: {e}")))?
                            .into_dyn()
                    }
                };
                inputs.push((format!("past_key_values.{layer}.{kv}"), t));
            }
        }

        let outputs = session
            .run(
                inputs
                    .iter()
                    .map(|(n, t)| (n.as_str(), t))
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| SemanticError::EmbeddingFailed(format!("DirectML run: {e}")))?;

        let out = outputs
            .get("last_hidden_state")
            .ok_or_else(|| SemanticError::EmbeddingFailed("no last_hidden_state output".into()))?;
        let (shape, data) = out
            .try_extract_tensor::<f32>()
            .map_err(|e| SemanticError::EmbeddingFailed(format!("extract hidden: {e}")))?;
        let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        if dims.len() != 3 || dims[0] != batch {
            return Err(SemanticError::EmbeddingFailed(format!(
                "unexpected hidden shape {dims:?}"
            )));
        }
        let (seq, hidden) = (dims[1], dims[2]);

        // Last-token pooling per item (last non-pad position), then L2 norm.
        let mut pooled: Vec<f32> = Vec::with_capacity(batch * hidden);
        for b in 0..batch {
            let row = &mask_flat[b * seq..(b + 1) * seq];
            let last_idx = row.iter().rposition(|&m| m == 1).unwrap_or(seq - 1);
            let base = (b * seq + last_idx) * hidden;
            let mut v: Vec<f32> = data[base..base + hidden].to_vec();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            pooled.extend_from_slice(&v);
        }
        Ok((pooled, real_tokens, hidden))
    }
}

impl SemanticProvider for OrtDirectMlProvider {
    fn id(&self) -> &'static str {
        ORT_PROVIDER_ID
    }
    fn model_id(&self) -> &str {
        crate::qwen3_provider::QWEN_MODEL_ID
    }
    fn dimensions(&self) -> usize {
        self.target_dims
    }
    fn max_input_bytes(&self) -> usize {
        self.seq_len * MIN_BYTES_PER_TOKEN
    }
    fn available(&self) -> bool {
        true
    }
    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        // DirectML sessions crash on concurrent Run() (onnxruntime #22147);
        // the mutex in run paths enforces this — never widen it.
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

        // Proactive VRAM admission (Phase 5): estimate this batch's cost and
        // check it against the current pressure/budget BEFORE submitting to
        // the device, rather than only reacting after a real OOM. Both a
        // required shrink and an outright rejection are reported the same
        // way the reactive OOM path already is (`BudgetExhausted`) — the
        // existing `oom_batch_cap` halving logic in `enrich.rs`'s drive loop
        // already handles that signal correctly, so this reuses it instead
        // of inventing a second batch-shrinking mechanism. Sustained
        // Critical pressure escalates to `ProviderUnavailable`, which the
        // fallback coordinator classifies as a permanent failure and acts on
        // immediately.
        let snapshot = vram_telemetry().current_snapshot();
        let bytes_per_item = attic_storage::gpu_telemetry::estimate_batch_bytes(
            1,
            self.seq_len,
            self.target_dims,
            NUM_LAYERS,
            2, // fp16
        );
        match vram_admission().admission_check(
            &snapshot,
            inputs.len(),
            bytes_per_item,
            Instant::now(),
        ) {
            AdmissionDecision::Admit { .. } => {}
            AdmissionDecision::Shrink { .. } | AdmissionDecision::Reject => {
                return Err(SemanticError::BudgetExhausted(
                    "VRAM admission: insufficient budget for this batch".into(),
                ));
            }
            AdmissionDecision::FallbackToCpu => {
                return Err(SemanticError::DevicePressure(
                    "sustained critical VRAM pressure".into(),
                ));
            }
        }

        let t0 = Instant::now();

        // Sort by estimated length so each sub-batch pads to its own widest
        // item (the measured 2.3× win), keeping (index, input) for reorder.
        let mut indexed: Vec<(usize, &EmbeddingInput)> = inputs.iter().enumerate().collect();
        indexed.sort_by_key(|(_, i)| i.text.len());

        let mut outputs: Vec<Option<EmbeddingOutput>> = (0..inputs.len()).map(|_| None).collect();
        let mut start = 0usize;
        while start < indexed.len() {
            if cancel.is_cancelled() {
                return Err(SemanticError::Cancelled {
                    completed: outputs.iter().flatten().count(),
                    total: inputs.len(),
                });
            }
            if let Some(dl) = deadline
                && Instant::now() >= dl
            {
                return Err(SemanticError::Cancelled {
                    completed: outputs.iter().flatten().count(),
                    total: inputs.len(),
                });
            }

            // Token-budgeted sub-batch: batch_items × seq_len ≤ budget, but
            // always admit at least one item so a single huge unit can never
            // deadlock the queue.
            let count_cap = start.saturating_add(self.batch_size).min(indexed.len());
            let mut end = start;
            while end < count_cap {
                let next_cost = (end + 1 - start) * self.seq_len;
                if end > start && next_cost > self.batch_token_budget {
                    break;
                }
                end += 1;
            }

            let texts: Vec<&str> = indexed[start..end]
                .iter()
                .map(|(_, i)| i.text.as_str())
                .collect();
            let (pooled, real_tokens, native_hidden) = {
                let mut guard = self
                    .session
                    .lock()
                    .map_err(|_| SemanticError::EmbeddingFailed("session mutex poisoned".into()))?;
                Self::run_forward(
                    &mut guard,
                    &self.tokenizer,
                    self.seq_len,
                    self.kv_dtype,
                    &texts,
                )?
            };

            // r04: rows in `pooled` are NATIVE width (e.g. 1024), regardless
            // of the configured target dimension. Slicing with the target
            // dimension here previously read wrong offsets for batch > 1.
            for (k, (orig_idx, input)) in indexed[start..end].iter().enumerate() {
                let mut vec = pooled[k * native_hidden..(k + 1) * native_hidden].to_vec();
                if vec.len() > self.target_dims {
                    vec.truncate(self.target_dims);
                    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for x in vec.iter_mut() {
                            *x /= norm;
                        }
                    }
                }
                outputs[*orig_idx] = Some(EmbeddingOutput {
                    unit_key: input.unit_key.clone(),
                    vector: vec,
                });
                usage.input_bytes += input.text.len() as u64;
                usage.items_embedded += 1;
            }
            let _ = real_tokens;
            start = end;
        }

        usage.elapsed_ms += t0.elapsed().as_millis() as u64;
        outputs
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| SemanticError::EmbeddingFailed("internal: unproduced output".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider};

    /// Spike-validated model directory (fp16 ONNX + tokenizer). Tests skip
    /// cleanly when the assets are not provisioned.
    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("ATTIC_ONNX_MODEL_DIR")
            .ok()
            .map(std::path::PathBuf::from)
            .filter(|p| p.join("model_fp16.onnx").is_file() && p.join("tokenizer.json").is_file())
    }

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    }

    /// r04 gate (real RTX A500): batched extraction must match single-item
    /// extraction at every Matryoshka dimension — regression coverage for the
    /// native-width row-stride bug that corrupted batches with dim < 1024.
    #[test]
    fn batch_matches_single_at_reduced_dimensions() {
        let Some(dir) = model_dir() else {
            eprintln!("ATTIC_ONNX_MODEL_DIR not provisioned; skipping hardware test");
            return;
        };
        let texts = [
            "fn alpha() { let x = 1; }",
            "a somewhat longer piece of source code with several tokens so padding differs",
            "tiny",
            "SELECT embedding FROM sem_embeddings WHERE vector_space_id = ?1",
        ];
        for dims in [256usize, 512, 1024] {
            let p = OrtDirectMlProvider::from_model_dir(&dir, 4, 512, Some(dims)).unwrap();
            let mut usage = ResourceUsage::default();
            let inputs: Vec<EmbeddingInput> = texts
                .iter()
                .map(|t| EmbeddingInput {
                    unit_key: (*t).to_string(),
                    text: (*t).to_string(),
                })
                .collect();
            let batched = p
                .embed_batch(&inputs, &CancelFlag::new(), &mut usage, None)
                .unwrap();
            for (i, t) in texts.iter().enumerate() {
                assert_eq!(batched[i].vector.len(), dims);
                let single = p
                    .embed_batch(
                        &[EmbeddingInput {
                            unit_key: (*t).to_string(),
                            text: (*t).to_string(),
                        }],
                        &CancelFlag::new(),
                        &mut usage,
                        None,
                    )
                    .unwrap();
                let sim = cosine(&batched[i].vector, &single[0].vector);
                assert!(
                    sim > 0.999,
                    "dims={dims} item {i}: batch vs single cosine {sim} — stride regression?"
                );
            }
        }
    }

    /// r04: input beyond the fixed token budget must fail with a typed
    /// error — the provider must never silently embed a truncated head.
    #[test]
    fn over_token_input_is_rejected_not_truncated() {
        let Some(dir) = model_dir() else {
            eprintln!("ATTIC_ONNX_MODEL_DIR not provisioned; skipping hardware test");
            return;
        };
        let p = OrtDirectMlProvider::from_model_dir(&dir, 4, 512, None).unwrap();
        let long = "embedding ".repeat(5_000);
        let mut usage = ResourceUsage::default();
        let err = p
            .embed_batch(
                &[EmbeddingInput {
                    unit_key: "long".into(),
                    text: long,
                }],
                &CancelFlag::new(),
                &mut usage,
                None,
            )
            .unwrap_err();
        assert!(
            matches!(err, SemanticError::InputTooManyTokens { tokens, max: 512 } if tokens > 512),
            "over-token input must be rejected with token counts, got {err:?}"
        );
    }
}
