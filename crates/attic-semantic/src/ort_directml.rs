//! ONNX Runtime + DirectML provider for Qwen3-Embedding-0.6B.
//!
//! Measured on an RTX A500 (4 GB VRAM, ORT 1.24 DirectML, fp16 export) over
//! the 13.4K real units of the attic repository:
//! - fixed 512-token padding, 8 items per pass: 7.6 chunks/s — 79–84% of
//!   every pass was padding (token p50 = 28, p90 = 421);
//! - length buckets + a 4096-token budget per pass: **18.9–25.4 chunks/s**,
//!   sustained for 9 minutes, host RSS flat, cosine ≥ 0.9999 vs fixed-512
//!   (same vector space — nothing needs re-embedding).
//!
//! Design, and the constraints behind it:
//! - **Length buckets.** Each input pads only to the smallest bucket
//!   (64/128/256/512/…, capped at `seq_len`) that holds it, and a pass
//!   carries `budget / bucket` items. Short code no longer pays for 512
//!   tokens of padding.
//! - **A fixed shape set.** DirectML compiles and keeps an arena allocation
//!   per distinct input shape; arbitrary batch sizes grew VRAM without bound
//!   (+2.2 → +2.9 GiB in the sustained run). Item counts are powers of two,
//!   so a bucket has at most log2(budget/bucket)+1 shapes ever.
//! - **Headroom, not ratios.** A shape already run reuses memory DirectML
//!   already holds, so it is always admitted. Only a NEW shape needs free
//!   VRAM; if the OS budget has no room, the pass steps down to a smaller or
//!   already-known shape, then waits briefly. VRAM pressure never rejects a
//!   batch and never demotes the device to CPU — the old ratio-based
//!   admission counted this process's own resident arena as "pressure" and
//!   drove a healthy GPU onto the CPU within ~30 s on a 4 GB card.
//! - **Thermal guard.** At `pause_c` (default 90 °C) no new pass is
//!   submitted until the core cools to `resume_c` (default 85 °C); one degree
//!   below `pause_c` the token budget halves. Sources and platform coverage
//!   are documented in `attic_storage::gpu_thermal`.
//! - **Failure isolation.** A failed multi-item pass is retried at half the
//!   size until the failing input is alone; only a single-item failure is
//!   returned, so one bad input cannot take a whole claim down with it.
//! - The onnx-community export is decoder-with-past: 28 layers × key/value
//!   `past_key_values.*` inputs must be fed as EMPTY tensors.
//! - DirectML sessions are NOT thread-safe for concurrent `Run()`
//!   (onnxruntime #22147): `ProviderConcurrencyContract::Serialized` plus a
//!   session mutex — never parallelize calls into one session.
//!
//! Tunables (read once per process, surfaced in `attic.toml` `[semantic]`
//! and forwarded to the inference worker as environment):
//! `ATTIC_GPU_BATCH_TOKENS`, `ATTIC_GPU_TEMP_PAUSE_C`, `ATTIC_GPU_TEMP_RESUME_C`.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use half::f16;
use ndarray::{Array2, Array4};
use ort::ep::DirectML;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use attic_inference_protocol::progress::{self, PauseGuard};
use attic_storage::gpu_telemetry::GpuTelemetry;
use attic_storage::gpu_thermal::{self, ThermalAction, ThermalGuard};

use crate::error::SemanticError;
use crate::instruction::CODE_RETRIEVAL_V1_ID;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ExecutionBackend,
    ProviderConcurrencyContract, ResourceUsage, SemanticProvider,
};

/// Provider id for the ORT/DirectML path.
pub const ORT_PROVIDER_ID: &str = "qwen3-ort";

/// Padded tokens per forward pass. 4096 measured best on a 4 GB card:
/// 8192 was only 3% faster and peaked at 3.9 of 4.0 GiB.
pub const DEFAULT_BATCH_TOKENS: usize = 4096;

/// Env var overriding [`DEFAULT_BATCH_TOKENS`].
pub use crate::worker_supervisor::ENV_GPU_BATCH_TOKENS as ENV_BATCH_TOKENS;
/// Env var overriding the thermal pause threshold (°C).
pub use crate::worker_supervisor::ENV_GPU_TEMP_PAUSE_C as ENV_TEMP_PAUSE_C;
/// Env var overriding the thermal resume threshold (°C).
pub use crate::worker_supervisor::ENV_GPU_TEMP_RESUME_C as ENV_TEMP_RESUME_C;

/// Smallest length bucket; every input pads to at least this many tokens.
/// 32 lets very short units (signatures, constants) fill 128-item passes
/// under the default 4096-token budget instead of padding to 64.
const MIN_BUCKET: usize = 32;

/// Free VRAM (MiB) kept untouched for the desktop/compositor when growing
/// DirectML's arena for a new shape.
const VRAM_RESERVE_MIB: u64 = 256;

/// How long to wait for headroom before running a new shape anyway (a real
/// device OOM is then reported as `BudgetExhausted`, which the drive loop
/// handles by halving its claim).
const PRESSURE_WAIT_MAX: Duration = Duration::from_secs(2);
const PRESSURE_POLL: Duration = Duration::from_millis(250);
const THERMAL_POLL: Duration = Duration::from_millis(500);
/// Longest a single batch waits for the GPU to cool before giving its items
/// back to the queue (a stuck sensor must not park the worker forever).
const MAX_THERMAL_PAUSE: Duration = Duration::from_secs(600);

/// Qwen3-Embedding-0.6B: 28 layers, 16 query heads, 8 KV heads, head_dim 128.
const NUM_LAYERS: usize = 28;
const NUM_Q_HEADS: u64 = 16;
const NUM_KV_HEADS: usize = 8;
const HEAD_DIM: usize = 128;
/// Model's native output width before Matryoshka truncation.
const NATIVE_DIMS: usize = 1024;
/// Floor bytes-per-token for conservative pre-tokenization estimates.
const MIN_BYTES_PER_TOKEN: usize = 2;

fn vram_telemetry() -> &'static GpuTelemetry {
    static TELEMETRY: OnceLock<GpuTelemetry> = OnceLock::new();
    TELEMETRY.get_or_init(GpuTelemetry::new)
}

fn env_parse<T: std::str::FromStr>(key: &str) -> Option<T> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Length buckets for a read window: 32, 64, … doubling, capped by (and
/// always ending at) `seq_len`.
pub(crate) fn length_buckets(seq_len: usize) -> Vec<usize> {
    let seq_len = seq_len.max(1);
    let mut out = Vec::new();
    let mut b = MIN_BUCKET;
    while b < seq_len {
        out.push(b);
        b *= 2;
    }
    out.push(seq_len);
    out
}

fn pow2_floor(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        1usize << (usize::BITS - 1 - n.leading_zeros())
    }
}

/// Items per full pass in `bucket` under `budget` padded tokens — a power of
/// two, at least 1 (a single maximum-length input always runs).
pub(crate) fn full_pass_items(budget: usize, bucket: usize) -> usize {
    pow2_floor((budget / bucket.max(1)).max(1))
}

/// Split `n` items into power-of-two pass sizes no larger than `cap`
/// (e.g. n=37, cap=32 → 32, 4, 1), so every pass is a member of the fixed
/// shape set.
pub(crate) fn pow2_passes(mut n: usize, cap: usize) -> Vec<usize> {
    let cap = pow2_floor(cap.max(1));
    let mut out = Vec::new();
    while n > 0 {
        let take = pow2_floor(n).min(cap);
        out.push(take);
        n -= take;
    }
    out
}

/// Conservative arena growth (MiB) for a new `(items, bucket)` shape:
/// per-token activations across the layer stack plus the attention score
/// matrix. Used only to decide whether a NEW shape can be admitted.
fn new_shape_cost_mib(items: usize, bucket: usize) -> u64 {
    let tokens = (items * bucket) as u64;
    let activations = tokens * NATIVE_DIMS as u64 * 2 * 24;
    let scores = items as u64 * NUM_Q_HEADS * (bucket as u64).pow(2) * 4;
    (activations + scores).div_ceil(1024 * 1024)
}

fn is_oom_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("out of memory")
        || m.contains("outofmemory")
        || m.contains("e_outofmemory")
        || m.contains("8007000e")
}

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
    /// Maximum tokens per input (the model read window).
    seq_len: usize,
    /// Padding buckets, ascending, last == `seq_len`.
    buckets: Vec<usize>,
    /// Padded tokens per forward pass.
    batch_token_budget: usize,
    target_dims: usize,
    fingerprint: EmbeddingFingerprint,
    kv_dtype: KvDType,
    thermal: ThermalGuard,
    /// `(items, bucket)` shapes DirectML already holds memory for.
    seen_shapes: Mutex<HashSet<(usize, usize)>>,
}

impl OrtDirectMlProvider {
    /// Build a provider from a local ONNX model directory containing
    /// `model_fp16.onnx` (+ its `.onnx_data`) and `tokenizer.json`.
    ///
    /// `seq_len` is the maximum tokens per input. `batch_size` only seeds the
    /// token budget when neither `ATTIC_GPU_BATCH_TOKENS` nor the default
    /// applies; the budget, not an item count, bounds each pass.
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
        let seq_len = seq_len.max(1);

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: ORT_PROVIDER_ID.into(),
                reason: format!("failed to load tokenizer: {e}"),
            }
        })?;
        // Padding is applied per bucket in `run_forward`, never by the
        // tokenizer. NO truncation either (r04): clipping an over-budget
        // input would store a vector claiming to represent the whole unit;
        // over-token inputs are rejected with InputTooManyTokens instead.
        tokenizer.with_padding(None);

        // No `sequence_length` dimension override: the session accepts every
        // bucket width. Shapes stay bounded by the fixed pass-size set.
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

        let batch_token_budget = env_parse::<usize>(ENV_BATCH_TOKENS)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_BATCH_TOKENS.max(batch_size.max(1) * MIN_BUCKET))
            .max(seq_len);
        let thermal = ThermalGuard::new(
            env_parse(ENV_TEMP_PAUSE_C).unwrap_or(gpu_thermal::DEFAULT_PAUSE_C),
            env_parse(ENV_TEMP_RESUME_C).unwrap_or(gpu_thermal::DEFAULT_RESUME_C),
        );
        let buckets = length_buckets(seq_len);
        tracing::info!(
            ?kv_dtype,
            seq_len,
            ?buckets,
            batch_token_budget,
            temp_pause_c = thermal.pause_c(),
            temp_resume_c = thermal.resume_c(),
            "DirectML embedding provider ready"
        );

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
            // Bucketed padding is not part of identity: causal attention
            // never lets right-padding reach a real token (cosine ≥ 0.9999
            // measured against fixed 512-token padding).
            quantization: "fp16-onnx".into(),
        };

        Ok(Self {
            session: Mutex::new(session),
            tokenizer,
            seq_len,
            buckets,
            batch_token_budget,
            target_dims,
            fingerprint,
            kv_dtype,
            thermal,
            seen_shapes: Mutex::new(HashSet::new()),
        })
    }

    fn bucket_for(&self, tokens: usize) -> usize {
        self.buckets
            .iter()
            .copied()
            .find(|b| tokens <= *b)
            .unwrap_or(self.seq_len)
    }

    fn shape_known(&self, items: usize, bucket: usize) -> bool {
        self.seen_shapes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(items, bucket))
    }

    fn remember_shape(&self, items: usize, bucket: usize) {
        self.seen_shapes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert((items, bucket));
    }

    fn forget_shape(&self, items: usize, bucket: usize) {
        self.seen_shapes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(items, bucket));
    }

    /// Block while the GPU is at or above the pause temperature. Returns the
    /// budget shift to apply (1 = halve while one degree below pause).
    ///
    /// The wait is reported to the supervisor as paused, so it is not charged
    /// to the batch's work budget; the caller extends its own deadline by the
    /// time spent here. Gives up on cancel or after [`MAX_THERMAL_PAUSE`].
    fn wait_thermal(&self, cancel: &CancelFlag) -> Result<u32, ()> {
        let mut logged = false;
        let mut pause: Option<(Instant, PauseGuard)> = None;
        loop {
            let temp = gpu_thermal::gpu_temperature_c();
            match self.thermal.decide(temp) {
                ThermalAction::Run => {
                    if logged {
                        tracing::info!(temp_c = ?temp, "GPU cooled; resuming embedding");
                    }
                    return Ok(0);
                }
                ThermalAction::Throttle => return Ok(1),
                ThermalAction::Pause => {
                    if !logged {
                        tracing::warn!(
                            temp_c = ?temp,
                            pause_c = self.thermal.pause_c(),
                            resume_c = self.thermal.resume_c(),
                            "GPU too hot; pausing embedding until it cools"
                        );
                        logged = true;
                    }
                    let since = pause
                        .get_or_insert_with(|| (Instant::now(), PauseGuard::enter()))
                        .0;
                    if cancel.is_cancelled() || since.elapsed() >= MAX_THERMAL_PAUSE {
                        return Err(());
                    }
                    std::thread::sleep(THERMAL_POLL);
                }
            }
        }
    }

    /// Largest admissible pass size ≤ `items` for `bucket`: a known shape is
    /// always admissible; a new one needs free VRAM beyond the reserve.
    /// Waits up to [`PRESSURE_WAIT_MAX`] for headroom before running the
    /// smallest shape regardless.
    fn admit_items(&self, items: usize, bucket: usize, cancel: &CancelFlag) -> usize {
        let waited = Instant::now();
        loop {
            let snap = vram_telemetry().current_snapshot();
            let mut n = items;
            loop {
                if self.shape_known(n, bucket) {
                    return n;
                }
                match snap.available_mib {
                    // Unknown telemetry: nothing to judge by — run it.
                    None => return n,
                    Some(free) if free >= new_shape_cost_mib(n, bucket) + VRAM_RESERVE_MIB => {
                        return n;
                    }
                    _ => {}
                }
                if n == 1 {
                    break;
                }
                n /= 2;
            }
            if cancel.is_cancelled() || waited.elapsed() >= PRESSURE_WAIT_MAX {
                tracing::debug!(bucket, "no VRAM headroom for a new shape; running 1 item");
                return 1;
            }
            let _paused = PauseGuard::enter();
            std::thread::sleep(PRESSURE_POLL);
        }
    }

    /// One forward pass over pre-tokenized rows, right-padded to `pad`.
    /// Caller holds the session mutex.
    ///
    /// Returns pooled, L2-normalized vectors at NATIVE width plus that width —
    /// row slicing MUST use the native width, never the configured target
    /// dimension (r04 stride fix).
    fn run_forward(
        session: &mut Session,
        kv_dtype: KvDType,
        rows: &[&[u32]],
        pad: usize,
    ) -> Result<(Vec<f32>, usize), SemanticError> {
        let batch = rows.len();
        let mut ids_flat: Vec<i64> = vec![0; batch * pad];
        let mut mask_flat: Vec<i64> = vec![0; batch * pad];
        for (r, ids) in rows.iter().enumerate() {
            let base = r * pad;
            for (k, &id) in ids.iter().enumerate() {
                ids_flat[base + k] = id as i64;
                mask_flat[base + k] = 1;
            }
        }
        let pos_flat: Vec<i64> = (0..batch).flat_map(|_| 0..pad as i64).collect();

        let to_tensor = |v: Vec<i64>| -> Result<Tensor<i64>, SemanticError> {
            let arr = Array2::from_shape_vec((batch, pad), v)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor shape: {e}")))?;
            Tensor::from_array(arr)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor: {e}")))
        };

        let mut inputs: Vec<(String, ort::value::DynValue)> = vec![
            ("input_ids".into(), to_tensor(ids_flat)?.into_dyn()),
            ("attention_mask".into(), to_tensor(mask_flat)?.into_dyn()),
            ("position_ids".into(), to_tensor(pos_flat)?.into_dyn()),
        ];
        for layer in 0..NUM_LAYERS {
            for kv in ["key", "value"] {
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
            .map_err(|e| {
                let msg = format!("DirectML run: {e}");
                if is_oom_message(&msg) {
                    SemanticError::BudgetExhausted(msg)
                } else {
                    SemanticError::EmbeddingFailed(msg)
                }
            })?;

        let out = outputs
            .get("last_hidden_state")
            .ok_or_else(|| SemanticError::EmbeddingFailed("no last_hidden_state output".into()))?;
        let (shape, data) = out
            .try_extract_tensor::<f32>()
            .map_err(|e| SemanticError::EmbeddingFailed(format!("extract hidden: {e}")))?;
        let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
        if dims.len() != 3 || dims[0] != batch || dims[1] != pad {
            return Err(SemanticError::EmbeddingFailed(format!(
                "unexpected hidden shape {dims:?}"
            )));
        }
        let hidden = dims[2];

        // Last-token pooling (last real position of each row), then L2 norm.
        let mut pooled: Vec<f32> = Vec::with_capacity(batch * hidden);
        for (b, ids) in rows.iter().enumerate() {
            let last_idx = ids.len().clamp(1, pad) - 1;
            let base = (b * pad + last_idx) * hidden;
            let mut v: Vec<f32> = data[base..base + hidden].to_vec();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in v.iter_mut() {
                    *x /= norm;
                }
            }
            pooled.extend_from_slice(&v);
        }
        Ok((pooled, hidden))
    }

    fn truncate_dims(&self, mut vec: Vec<f32>) -> Vec<f32> {
        if vec.len() > self.target_dims {
            vec.truncate(self.target_dims);
            let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in vec.iter_mut() {
                    *x /= norm;
                }
            }
        }
        vec
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
    fn preferred_claim_items(&self) -> Option<usize> {
        Some(crate::worker_supervisor::GPU_CLAIM_ITEMS)
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let cancelled = |done: usize| SemanticError::Cancelled {
            completed: done,
            total: inputs.len(),
        };
        if cancel.is_cancelled() {
            return Err(cancelled(0));
        }
        let t0 = Instant::now();
        let mut deadline = deadline;

        let texts: Vec<&str> = inputs.iter().map(|i| i.text.as_str()).collect();
        let encodings = self
            .tokenizer
            .encode_batch(texts, true)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tokenize: {e}")))?;
        let token_ids: Vec<&[u32]> = encodings.iter().map(|e| e.get_ids()).collect();
        if let Some(too_long) = token_ids.iter().find(|ids| ids.len() > self.seq_len) {
            return Err(SemanticError::InputTooManyTokens {
                tokens: too_long.len(),
                max: self.seq_len,
            });
        }

        // Group input indices by bucket (ascending), preserving order.
        let mut by_bucket: Vec<(usize, Vec<usize>)> =
            self.buckets.iter().map(|&b| (b, Vec::new())).collect();
        for (idx, ids) in token_ids.iter().enumerate() {
            let b = self.bucket_for(ids.len());
            if let Some((_, v)) = by_bucket.iter_mut().find(|(bb, _)| *bb == b) {
                v.push(idx);
            }
        }

        let mut outputs: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        let mut done = 0usize;
        for (bucket, members) in by_bucket.into_iter().filter(|(_, m)| !m.is_empty()) {
            // Per-call cap for this bucket; halves when a pass fails so the
            // failing input is isolated in at most log2(cap) extra passes.
            let mut cap = full_pass_items(self.batch_token_budget, bucket);
            let mut start = 0usize;
            while start < members.len() {
                if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(cancelled(done));
                }
                // Waits below are paused (not charged by the supervisor), so
                // they must not eat into this batch's own deadline either.
                let waits = Instant::now();
                let shift = self.wait_thermal(cancel).map_err(|_| cancelled(done))?;
                let budget_cap = (full_pass_items(self.batch_token_budget, bucket) >> shift).max(1);
                let want = pow2_passes(members.len() - start, cap.min(budget_cap))[0];
                let items = self.admit_items(want, bucket, cancel);
                deadline = deadline.map(|d| d + waits.elapsed());
                let chunk = &members[start..start + items];
                let rows: Vec<&[u32]> = chunk.iter().map(|&i| token_ids[i]).collect();

                let result = {
                    let mut guard = self.session.lock().map_err(|_| {
                        SemanticError::EmbeddingFailed("session mutex poisoned".into())
                    })?;
                    Self::run_forward(&mut guard, self.kv_dtype, &rows, bucket)
                };
                progress::tick();
                match result {
                    Ok((pooled, native_hidden)) => {
                        self.remember_shape(items, bucket);
                        // r04: rows in `pooled` are NATIVE width regardless of
                        // the configured target dimension.
                        for (k, &orig) in chunk.iter().enumerate() {
                            let v = pooled[k * native_hidden..(k + 1) * native_hidden].to_vec();
                            outputs[orig] = Some(self.truncate_dims(v));
                            usage.input_bytes += inputs[orig].text.len() as u64;
                            usage.items_embedded += 1;
                        }
                        done += items;
                        start += items;
                    }
                    Err(e) if items > 1 => {
                        tracing::warn!(
                            items,
                            bucket,
                            "DirectML pass failed ({e}); retrying at half size"
                        );
                        self.forget_shape(items, bucket);
                        cap = items / 2;
                    }
                    Err(e) => return Err(e),
                }
            }
        }

        usage.elapsed_ms += t0.elapsed().as_millis() as u64;
        outputs
            .into_iter()
            .zip(inputs)
            .map(|(v, input)| {
                v.map(|vector| EmbeddingOutput {
                    unit_key: input.unit_key.clone(),
                    vector,
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| SemanticError::EmbeddingFailed("internal: unproduced output".into()))
    }
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    #[test]
    fn buckets_double_and_end_at_window() {
        assert_eq!(length_buckets(512), vec![32, 64, 128, 256, 512]);
        assert_eq!(length_buckets(1024), vec![32, 64, 128, 256, 512, 1024]);
        assert_eq!(length_buckets(300), vec![32, 64, 128, 256, 300]);
        assert_eq!(length_buckets(32), vec![32]);
        assert_eq!(length_buckets(20), vec![20]);
    }

    #[test]
    fn full_pass_is_power_of_two_within_budget() {
        assert_eq!(full_pass_items(4096, 32), 128);
        assert_eq!(full_pass_items(4096, 64), 64);
        assert_eq!(full_pass_items(4096, 512), 8);
        assert_eq!(full_pass_items(4096, 300), 8);
        assert_eq!(full_pass_items(256, 512), 1);
    }

    #[test]
    fn passes_use_only_power_of_two_sizes() {
        assert_eq!(pow2_passes(37, 32), vec![32, 4, 1]);
        assert_eq!(pow2_passes(100, 64), vec![64, 32, 4]);
        assert_eq!(pow2_passes(5, 48), vec![4, 1]);
        assert!(pow2_passes(0, 8).is_empty());
    }

    #[test]
    fn oom_messages_are_recognised() {
        assert!(is_oom_message("DirectML run: E_OUTOFMEMORY"));
        assert!(is_oom_message("failed with HRESULT 0x8007000E"));
        assert!(!is_oom_message("failed: 0x887A0005 device removed"));
        assert!(!is_oom_message("invalid input shape"));
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

    /// Bucketed padding must stay in the same vector space as the historical
    /// fixed 512-token padding, or every stored vector would need re-embedding.
    #[test]
    fn bucket_padding_matches_fixed_512_padding() {
        let Some(dir) = model_dir() else {
            eprintln!("ATTIC_ONNX_MODEL_DIR not provisioned; skipping hardware test");
            return;
        };
        let p = OrtDirectMlProvider::from_model_dir(&dir, 8, 512, None).unwrap();
        let texts = [
            "fn alpha() { let x = 1; }",
            "SELECT embedding FROM sem_embeddings WHERE vector_space_id = ?1 ORDER BY id",
        ];
        for t in texts {
            let enc = p.tokenizer.encode(t, true).unwrap();
            let ids = enc.get_ids();
            let bucket = p.bucket_for(ids.len());
            let mut s = p.session.lock().unwrap();
            let (a, _) = OrtDirectMlProvider::run_forward(&mut s, p.kv_dtype, &[ids], bucket).unwrap();
            let (b, _) = OrtDirectMlProvider::run_forward(&mut s, p.kv_dtype, &[ids], 512).unwrap();
            let sim = cosine(&a, &b);
            assert!(sim > 0.999, "bucket {bucket} vs 512 cosine {sim}");
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
