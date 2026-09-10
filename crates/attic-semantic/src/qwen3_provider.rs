//! `Qwen3Embedder` — a Candle-backed `EmbeddingProvider` and `SemanticProvider`
//! for `Qwen/Qwen3-Embedding-0.6B` (Master Plan V2 §29–§32, CP7).
//!
//! Pipeline:
//! ```text
//! tokenize (BatchLongest) → Qwen2 transformer forward pass →
//! Last-token (or Mean) pooling → L2 normalize → Matryoshka slice (512/768/1024) →
//! L2 re-normalize
//! ```
//!
//! Queries are prepended with `CODE_RETRIEVAL_V1_TEMPLATE` while documents are embedded
//! without modification (§30, §31).
//! Thread safety is guaranteed via `std::sync::Mutex<Model>`.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use crate::error::SemanticError;
use crate::instruction::{CODE_RETRIEVAL_V1_ID, format_query_instruction};
use crate::provider::{
    CancelFlag, EmbeddingExecutionBudget, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput,
    EmbeddingProvider, ResourceUsage, SemanticProvider,
};
use crate::qwen3_model::{Qwen3Config, Qwen3Model};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

pub const HF_QWEN_OWNER: &str = "Qwen";
pub const HF_QWEN_REPO: &str = "Qwen3-Embedding-0.6B";
pub const QWEN_PROVIDER_ID: &str = "qwen3";
pub const QWEN_MODEL_ID: &str = "qwen3-embedding-0.6b";
pub const DEFAULT_MAX_TOKENS: usize = 512;
pub const NATIVE_QWEN_DIMENSION: usize = 1024;
const DTYPE: DType = DType::F32;

/// Supported pooling strategies for Qwen embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QwenPooling {
    /// Extract the final non-padded token representation.
    #[default]
    LastToken,
    /// Compute the attention-mask-weighted mean across sequence tokens.
    Mean,
}

impl QwenPooling {
    pub fn as_version_str(&self) -> &'static str {
        match self {
            Self::LastToken => "last_token_v1",
            Self::Mean => "mean_v1",
        }
    }
}

/// A real, Candle-backed provider for Qwen3 embeddings.
pub struct Qwen3Embedder {
    model: Mutex<Qwen3Model>,
    tokenizer: Tokenizer,
    device: Device,
    batch_size: usize,
    native_dims: usize,
    target_dims: usize,
    max_tokens: usize,
    pooling: QwenPooling,
    fingerprint: EmbeddingFingerprint,
}

impl Qwen3Embedder {
    /// Native hidden dimensionality of the model before Matryoshka truncation.
    pub fn native_dims(&self) -> usize {
        self.native_dims
    }

    /// Update target Matryoshka dimension without reloading model tensors.
    pub fn set_target_dims(&mut self, dims: usize) {
        assert!(
            dims > 0 && dims <= self.native_dims,
            "target dimension must be <= native dims"
        );
        self.target_dims = dims;
        self.fingerprint.dimension = dims;
    }

    /// Construct a `Qwen3Embedder` from a local cache directory or Hugging Face.
    pub fn new(
        cache_dir: &Path,
        batch_size: usize,
        dimension_override: Option<usize>,
        pooling: QwenPooling,
    ) -> Result<Self, SemanticError> {
        if let Some((config, tokenizer, weights, revision)) = Self::try_local_cache(cache_dir, None)
        {
            return Self::build(
                config,
                tokenizer,
                weights,
                revision,
                batch_size,
                dimension_override,
                pooling,
            );
        }
        Self::download_and_build(cache_dir, batch_size, None, dimension_override, pooling)
    }

    /// Construct a `Qwen3Embedder` exclusively from a local cache directory.
    /// Fails immediately if model files are not present, with zero network calls.
    pub fn from_local_cache(
        cache_dir: &Path,
        batch_size: usize,
        dimension_override: Option<usize>,
        pooling: QwenPooling,
    ) -> Result<Self, SemanticError> {
        if let Some((config, tokenizer, weights, revision)) = Self::try_local_cache(cache_dir, None)
        {
            return Self::build(
                config,
                tokenizer,
                weights,
                revision,
                batch_size,
                dimension_override,
                pooling,
            );
        }
        Err(SemanticError::ProviderUnavailable {
            provider: QWEN_PROVIDER_ID.into(),
            reason: format!(
                "Qwen3 model weights not found in local cache '{}'",
                cache_dir.display()
            ),
        })
    }

    /// Construct a `Qwen3Embedder` pinned to an exact commit revision.
    pub fn new_pinned(
        cache_dir: &Path,
        batch_size: usize,
        revision: &str,
        dimension_override: Option<usize>,
        pooling: QwenPooling,
    ) -> Result<Self, SemanticError> {
        if let Some((config, tokenizer, weights, revision)) =
            Self::try_local_cache(cache_dir, Some(revision))
        {
            return Self::build(
                config,
                tokenizer,
                weights,
                revision,
                batch_size,
                dimension_override,
                pooling,
            );
        }
        Self::download_and_build(
            cache_dir,
            batch_size,
            Some(revision),
            dimension_override,
            pooling,
        )
    }

    /// Check `hf-hub`'s standard on-disk cache layout directly.
    pub fn try_local_cache(
        cache_dir: &Path,
        pinned_revision: Option<&str>,
    ) -> Option<(PathBuf, PathBuf, PathBuf, String)> {
        let repo_dir = cache_dir.join(format!("models--{HF_QWEN_OWNER}--{HF_QWEN_REPO}"));
        let revision = match pinned_revision {
            Some(r) => r.to_string(),
            None => std::fs::read_to_string(repo_dir.join("refs").join("main"))
                .ok()?
                .trim()
                .to_string(),
        };
        let snapshot = repo_dir.join("snapshots").join(&revision);
        let config = snapshot.join("config.json");
        let tokenizer = snapshot.join("tokenizer.json");
        let weights = snapshot.join("model.safetensors");
        if config.is_file() && tokenizer.is_file() && weights.is_file() {
            Some((config, tokenizer, weights, revision))
        } else {
            None
        }
    }

    /// Download model files via `hf-hub` client and construct the embedder.
    fn download_and_build(
        cache_dir: &Path,
        batch_size: usize,
        pinned_revision: Option<&str>,
        dimension_override: Option<usize>,
        pooling: QwenPooling,
    ) -> Result<Self, SemanticError> {
        let client = hf_hub::HFClient::builder()
            .cache_dir(cache_dir.to_path_buf())
            .build_sync()
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to build hf-hub client: {e}"),
            })?;
        let repo = client.model(HF_QWEN_OWNER.to_string(), HF_QWEN_REPO.to_string());

        let resolved_revision = match pinned_revision {
            Some(r) => r.to_string(),
            None => {
                let info = repo
                    .info()
                    .send()
                    .map_err(|e| SemanticError::ProviderUnavailable {
                        provider: QWEN_PROVIDER_ID.into(),
                        reason: format!(
                            "failed to resolve {HF_QWEN_OWNER}/{HF_QWEN_REPO} revision: {e}"
                        ),
                    })?;
                info.sha.ok_or_else(|| SemanticError::ProviderUnavailable {
                    provider: QWEN_PROVIDER_ID.into(),
                    reason: format!(
                        "{HF_QWEN_OWNER}/{HF_QWEN_REPO} repo info did not include a commit sha"
                    ),
                })?
            }
        };

        let config_path = Self::fetch(&repo, "config.json", &resolved_revision)?;
        let tokenizer_path = Self::fetch(&repo, "tokenizer.json", &resolved_revision)?;
        let weights_path = Self::fetch(&repo, "model.safetensors", &resolved_revision)?;

        Self::build(
            config_path,
            tokenizer_path,
            weights_path,
            resolved_revision,
            batch_size,
            dimension_override,
            pooling,
        )
    }

    fn fetch(
        repo: &hf_hub::HFRepositorySync<hf_hub::RepoTypeModel>,
        filename: &str,
        revision: &str,
    ) -> Result<PathBuf, SemanticError> {
        repo.download_file()
            .filename(filename)
            .revision(revision)
            .send()
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!(
                    "failed to fetch {filename} from {HF_QWEN_OWNER}/{HF_QWEN_REPO}@{revision}: {e}"
                ),
            })
    }

    /// Build embedder from resolved local file paths.
    pub fn build(
        config_path: PathBuf,
        tokenizer_path: PathBuf,
        weights_path: PathBuf,
        resolved_revision: String,
        batch_size: usize,
        dimension_override: Option<usize>,
        pooling: QwenPooling,
    ) -> Result<Self, SemanticError> {
        let config_str = std::fs::read_to_string(&config_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to read {}: {e}", config_path.display()),
            }
        })?;

        let qwen_config: Qwen3Config =
            serde_json::from_str(&config_str).map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to decode Qwen3Config: {e}"),
            })?;

        let native_dims = qwen_config.hidden_size;
        let target_dims = dimension_override.unwrap_or(native_dims);
        if target_dims == 0 || target_dims > native_dims {
            return Err(SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!(
                    "target dimension {target_dims} invalid (must be between 1 and native {native_dims})"
                ),
            });
        }

        let max_tokens = qwen_config.max_position_embeddings.min(DEFAULT_MAX_TOKENS);

        let device = Device::Cpu;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device) }
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to load model weights: {e}"),
            })?;

        let model =
            Qwen3Model::new(&qwen_config, vb).map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to construct Qwen3Model: {e}"),
            })?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to load tokenizer: {e}"),
            }
        })?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            ..Default::default()
        }));
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: max_tokens,
                ..Default::default()
            }))
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: QWEN_PROVIDER_ID.into(),
                reason: format!("failed to configure tokenizer truncation: {e}"),
            })?;

        let fingerprint = EmbeddingFingerprint {
            provider: QWEN_PROVIDER_ID.into(),
            model_id: QWEN_MODEL_ID.into(),
            model_revision: resolved_revision,
            dimension: target_dims,
            pooling_version: pooling.as_version_str().to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "qwen_bpe_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: CODE_RETRIEVAL_V1_ID.to_string(),
        };

        Ok(Self {
            model: Mutex::new(model),
            tokenizer,
            device,
            batch_size: batch_size.max(1),
            native_dims,
            target_dims,
            max_tokens,
            pooling,
            fingerprint,
        })
    }

    /// Access the underlying tokenizer.
    pub fn tokenizer(&self) -> &tokenizers::Tokenizer {
        &self.tokenizer
    }

    /// Extract last non-padding token representation for each item in the batch.
    pub fn last_token_pool(
        hidden_states: &Tensor,
        attention_mask: &[Vec<u32>],
    ) -> candle_core::Result<Tensor> {
        let (batch_size, seq_len, _hidden_size) = hidden_states.dims3()?;
        let mut pooled_items: Vec<Tensor> = Vec::with_capacity(batch_size);

        for (b, mask_row) in attention_mask.iter().enumerate() {
            // Find the last index where token is active (mask == 1)
            let mut last_idx = 0;
            for (idx, &val) in mask_row.iter().enumerate() {
                if val > 0 && idx < seq_len {
                    last_idx = idx;
                }
            }
            let item_vec = hidden_states.i((b, last_idx, ..))?; // shape: (hidden_size)
            pooled_items.push(item_vec.unsqueeze(0)?); // shape: (1, hidden_size)
        }

        Tensor::cat(&pooled_items, 0) // shape: (batch_size, hidden_size)
    }

    /// Compute attention-mask-weighted mean across sequence tokens.
    pub fn mean_pool(
        hidden_states: &Tensor,
        attention_mask_tensor: &Tensor,
    ) -> candle_core::Result<Tensor> {
        // hidden_states: (batch, seq, hidden)
        // attention_mask_tensor: (batch, seq)
        let mask_f32 = attention_mask_tensor.to_dtype(DType::F32)?;
        let mask_expanded = mask_f32.unsqueeze(2)?; // (batch, seq, 1)

        let weighted = hidden_states.broadcast_mul(&mask_expanded)?; // (batch, seq, hidden)
        let sum_hidden = weighted.sum(1)?; // (batch, hidden)
        let sum_mask = mask_f32.sum_keepdim(1)?.clamp(1e-9, f32::MAX)?; // (batch, 1)

        sum_hidden.broadcast_div(&sum_mask) // (batch, hidden)
    }

    /// L2-normalize tensor along dimension 1.
    pub fn l2_normalize(tensor: &Tensor) -> candle_core::Result<Tensor> {
        let norm = tensor.sqr()?.sum_keepdim(1)?.sqrt()?;
        let clamped_norm = norm.clamp(1e-12, f32::MAX)?;
        tensor.broadcast_div(&clamped_norm)
    }

    /// Apply Matryoshka dimension truncation and re-normalize.
    pub fn apply_matryoshka(tensor: &Tensor, target_dim: usize) -> candle_core::Result<Tensor> {
        let current_dim = tensor.dim(1)?;
        if target_dim < current_dim {
            let sliced = tensor.narrow(1, 0, target_dim)?;
            Self::l2_normalize(&sliced)
        } else {
            Ok(tensor.clone())
        }
    }

    /// Forward pass through model, pooling, normalization, and dimension truncation.
    fn embed_sub_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SemanticError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tokenization failed: {e}")))?;

        let token_ids: Vec<Vec<u32>> = encodings.iter().map(|e| e.get_ids().to_vec()).collect();
        let attention_mask_rows: Vec<Vec<u32>> = encodings
            .iter()
            .map(|e| e.get_attention_mask().to_vec())
            .collect();

        let token_ids_tensor = Tensor::new(token_ids, &self.device)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor build failed: {e}")))?;
        let attention_mask_tensor = Tensor::new(attention_mask_rows.clone(), &self.device)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor build failed: {e}")))?;

        let seq_len = token_ids_tensor.dim(1).map_err(|e| {
            SemanticError::EmbeddingFailed(format!("failed to get sequence length: {e}"))
        })?;
        let causal_mask =
            Qwen3Model::build_attention_mask(&attention_mask_rows, seq_len, &self.device)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("mask build failed: {e}")))?;

        let hidden_states = {
            let model_guard = self
                .model
                .lock()
                .map_err(|_| SemanticError::EmbeddingFailed("model mutex poisoned".to_string()))?;
            model_guard
                .forward(&token_ids_tensor, Some(&causal_mask))
                .map_err(|e| {
                    SemanticError::EmbeddingFailed(format!("Qwen3 forward pass failed: {e}"))
                })?
        };

        let pooled = match self.pooling {
            QwenPooling::LastToken => Self::last_token_pool(&hidden_states, &attention_mask_rows)
                .map_err(|e| {
                SemanticError::EmbeddingFailed(format!("last-token pooling failed: {e}"))
            })?,
            QwenPooling::Mean => Self::mean_pool(&hidden_states, &attention_mask_tensor)
                .map_err(|e| SemanticError::EmbeddingFailed(format!("mean pooling failed: {e}")))?,
        };

        let normalized = Self::l2_normalize(&pooled)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("normalization failed: {e}")))?;

        let truncated = Self::apply_matryoshka(&normalized, self.target_dims).map_err(|e| {
            SemanticError::EmbeddingFailed(format!("matryoshka truncation failed: {e}"))
        })?;

        truncated
            .to_dtype(DType::F32)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("dtype conversion failed: {e}")))?
            .to_vec2()
            .map_err(|e| SemanticError::EmbeddingFailed(format!("vector extraction failed: {e}")))
    }
}

impl SemanticProvider for Qwen3Embedder {
    fn id(&self) -> &'static str {
        QWEN_PROVIDER_ID
    }

    fn model_id(&self) -> &str {
        QWEN_MODEL_ID
    }

    fn dimensions(&self) -> usize {
        self.target_dims
    }

    fn max_input_bytes(&self) -> usize {
        self.max_tokens * 64
    }

    fn available(&self) -> bool {
        true
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
        let t0 = Instant::now();
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        for item in inputs {
            if item.text.len() > self.max_input_bytes() {
                return Err(SemanticError::InputTooLarge {
                    len: item.text.len(),
                    max: self.max_input_bytes(),
                });
            }
        }

        let mut indexed: Vec<(usize, &EmbeddingInput)> = inputs.iter().enumerate().collect();
        indexed.sort_by_key(|(_, item)| item.text.len());

        let mut sorted_outputs: Vec<(usize, EmbeddingOutput)> = Vec::with_capacity(inputs.len());

        for chunk in indexed.chunks(self.batch_size) {
            if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SemanticError::Cancelled {
                    completed: sorted_outputs.len(),
                    total: inputs.len(),
                });
            }

            let texts: Vec<&str> = chunk.iter().map(|(_, item)| item.text.as_str()).collect();
            let vectors = self.embed_sub_batch(&texts)?;

            for ((orig_idx, item), vector) in chunk.iter().zip(vectors) {
                if vector.len() != self.target_dims || vector.iter().any(|v| !v.is_finite()) {
                    return Err(SemanticError::EmbeddingFailed(format!(
                        "provider produced an invalid vector for unit '{}' (len={}, expected={})",
                        item.unit_key,
                        vector.len(),
                        self.target_dims
                    )));
                }
                sorted_outputs.push((
                    *orig_idx,
                    EmbeddingOutput {
                        unit_key: item.unit_key.clone(),
                        vector,
                    },
                ));
            }
        }

        sorted_outputs.sort_by_key(|(orig_idx, _)| *orig_idx);
        let outputs: Vec<EmbeddingOutput> = sorted_outputs.into_iter().map(|(_, o)| o).collect();

        let total_bytes: usize = inputs.iter().map(|i| i.text.len()).sum();
        let elapsed = t0.elapsed();
        usage.merge(&ResourceUsage {
            items_embedded: outputs.len() as u64,
            input_bytes: total_bytes as u64,
            elapsed_ms: elapsed.as_millis().max(1) as u64,
        });

        Ok(outputs)
    }
}

impl EmbeddingProvider for Qwen3Embedder {
    fn model_fingerprint(&self) -> EmbeddingFingerprint {
        self.fingerprint.clone()
    }

    fn dimension(&self) -> usize {
        self.target_dims
    }

    fn warm_up(&self, _budget: &EmbeddingExecutionBudget) -> Result<(), SemanticError> {
        let dummy = "/* warm up */";
        let _ = self.embed_sub_batch(&[dummy])?;
        Ok(())
    }

    fn embed_documents(
        &self,
        inputs: &[EmbeddingInput],
        budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        self.embed_batch(inputs, &cancel, &mut usage, budget.deadline)
    }

    fn embed_query(
        &self,
        query: &str,
        _budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<f32>, SemanticError> {
        let instructed = format_query_instruction(CODE_RETRIEVAL_V1_ID, query);
        if instructed.len() > self.max_input_bytes() {
            return Err(SemanticError::InputTooLarge {
                len: instructed.len(),
                max: self.max_input_bytes(),
            });
        }
        let mut vectors = self.embed_sub_batch(&[&instructed])?;
        vectors.pop().ok_or_else(|| {
            SemanticError::EmbeddingFailed("empty query embedding output".to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_token_pool_extracts_correct_positions() {
        let device = Device::Cpu;
        // Batch size 2, seq_len 4, hidden_size 3
        // Sequence 0: length 2 (mask: [1, 1, 0, 0])
        // Sequence 1: length 4 (mask: [1, 1, 1, 1])
        let data: Vec<f32> = vec![
            // Seq 0: tokens 0, 1, 2, 3
            1.0, 1.0, 1.0, // token 0
            2.0, 2.0, 2.0, // token 1 (last active!)
            9.0, 9.0, 9.0, // token 2 (padding)
            9.0, 9.0, 9.0, // token 3 (padding)
            // Seq 1: tokens 0, 1, 2, 3
            3.0, 3.0, 3.0, // token 0
            4.0, 4.0, 4.0, // token 1
            5.0, 5.0, 5.0, // token 2
            6.0, 6.0, 6.0, // token 3 (last active!)
        ];
        let hidden = Tensor::from_vec(data, (2, 4, 3), &device).unwrap();
        let mask = vec![vec![1u32, 1, 0, 0], vec![1u32, 1, 1, 1]];

        let pooled = Qwen3Embedder::last_token_pool(&hidden, &mask).unwrap();
        assert_eq!(pooled.dims(), &[2, 3]);
        let pooled_vec = pooled.to_vec2::<f32>().unwrap();
        assert_eq!(pooled_vec[0], vec![2.0, 2.0, 2.0]);
        assert_eq!(pooled_vec[1], vec![6.0, 6.0, 6.0]);
    }

    #[test]
    fn mean_pool_averages_over_active_tokens() {
        let device = Device::Cpu;
        // Batch size 1, seq_len 3, hidden_size 2
        let data: Vec<f32> = vec![
            10.0, 20.0, // token 0
            20.0, 40.0, // token 1
            99.0, 99.0, // token 2 (padding)
        ];
        let hidden = Tensor::from_vec(data, (1, 3, 2), &device).unwrap();
        let mask = Tensor::new(vec![vec![1u32, 1, 0]], &device).unwrap();

        let pooled = Qwen3Embedder::mean_pool(&hidden, &mask).unwrap();
        assert_eq!(pooled.dims(), &[1, 2]);
        let pooled_vec = pooled.to_vec2::<f32>().unwrap();
        assert!((pooled_vec[0][0] - 15.0).abs() < 1e-5);
        assert!((pooled_vec[0][1] - 30.0).abs() < 1e-5);
    }

    #[test]
    fn l2_normalize_produces_unit_length() {
        let device = Device::Cpu;
        let data = vec![3.0f32, 4.0]; // Norm is 5.0
        let t = Tensor::from_vec(data, (1, 2), &device).unwrap();
        let normalized = Qwen3Embedder::l2_normalize(&t).unwrap();
        let vec = normalized.to_vec2::<f32>().unwrap();
        assert!((vec[0][0] - 0.6).abs() < 1e-5);
        assert!((vec[0][1] - 0.8).abs() < 1e-5);
        let norm = (vec[0][0].powi(2) + vec[0][1].powi(2)).sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn matryoshka_truncation_rescales_and_normalizes() {
        let device = Device::Cpu;
        // 4D vector, truncate to 2D
        let data = vec![1.0f32, 1.0, 1.0, 1.0];
        let t = Tensor::from_vec(data, (1, 4), &device).unwrap();
        let normalized = Qwen3Embedder::l2_normalize(&t).unwrap();
        let truncated = Qwen3Embedder::apply_matryoshka(&normalized, 2).unwrap();
        assert_eq!(truncated.dims(), &[1, 2]);
        let vec = truncated.to_vec2::<f32>().unwrap();
        let norm = (vec[0][0].powi(2) + vec[0][1].powi(2)).sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "truncated vector must be re-normalized to 1.0"
        );
    }

    #[test]
    fn query_instruction_distinction() {
        let query = "select * from users";
        let formatted = format_query_instruction(CODE_RETRIEVAL_V1_ID, query);
        assert!(formatted.starts_with("Instruct: Given a code search query"));
        assert!(formatted.ends_with("Query: select * from users"));
    }
}
