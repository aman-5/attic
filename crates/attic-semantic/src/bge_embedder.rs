//! `BgeEmbedder` — a real, Candle-backed `SemanticProvider` for
//! `BAAI/bge-base-en-v1.5` (Phase 9).
//!
//! [FIX — round 17] Deliberately `bge-base` (768-dim), not `bge-small`
//! (384-dim) or `bge-large` (1024-dim) — a fixed choice, not a configurable
//! one (see `EmbeddingOverride`'s doc comment for why `model` isn't a
//! user-tunable knob).
//!
//! Candle's `BertModel` supplies the transformer encoder only (raw hidden
//! states); this module implements BGE's specific pipeline on top:
//!
//! ```text
//! tokenize → BertModel forward pass → CLS-pool (index 0 of the sequence
//! dimension) → L2-normalize → 768-dim vector
//! ```
//!
//! Verified against `BAAI/bge-base-en-v1.5`'s actual `config.json` on
//! 2026-09-03: `architectures: ["BertModel"]` (a standard BERT encoder, not a
//! custom architecture), `hidden_size: 768`, `max_position_embeddings: 512`,
//! `pad_token_id: 0`. Model and tokenizer files are resolved to an immutable
//! commit SHA via `hf-hub` before hashing into an `EmbeddingSpaceDescriptor`
//! (Low-Level Design §3) — never the mutable ref `"main"`. Both artifacts
//! live in the same Hugging Face repo, so they resolve to the same commit
//! SHA here; the descriptor still records them as two independently-named
//! fields per the design, rather than assuming one from the other.

use std::path::{Path, PathBuf};
use std::time::Instant;

use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use crate::embedding_profile::{EmbeddingSpaceDescriptor, PoolingStrategy, TruncationPolicy};
use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingInput, EmbeddingOutput, ResourceUsage, SemanticProvider,
};

/// Repo hosting both the model weights and the tokenizer, verified against
/// the real `hf-hub` 1.0.0 API (a full async/blocking rewrite from the
/// pre-1.0 `Api`/`ApiRepo` shape — checked against the actual downloaded
/// crate source on 2026-09-03, not assumed from crates.io metadata alone).
const HF_OWNER: &str = "BAAI";
const HF_MODEL_REPO: &str = "bge-base-en-v1.5";
/// Dtype used throughout the forward pass. Defined locally rather than
/// relying on an uncertain re-export from `candle_transformers::models::bert`.
const DTYPE: DType = DType::F32;
/// Verified against the model's real `config.json` (see module docs).
const EXPECTED_HIDDEN_SIZE: usize = 768;
const EXPECTED_MAX_POSITION_EMBEDDINGS: usize = 512;

/// A real, Candle-backed `SemanticProvider` for `bge-base-en-v1.5`.
pub struct BgeEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    batch_size: usize,
    dims: usize,
    max_tokens: usize,
    descriptor: EmbeddingSpaceDescriptor,
}

impl BgeEmbedder {
    pub const PROVIDER_ID: &'static str = "bge";
    pub const MODEL_NAME: &'static str = "bge-base-en-v1.5";

    /// Construct a `BgeEmbedder` for `BAAI/bge-base-en-v1.5`. Checks the
    /// local cache directly (via `hf-hub`'s own on-disk layout — verified by
    /// direct inspection: `models--{owner}--{repo}/refs/main` +
    /// `snapshots/<revision>/`) before ever touching the network. If the
    /// model is already fully cached from a previous run, this is 100%
    /// offline. Only downloads (a network call to resolve the current
    /// revision, then fetch files) when nothing usable is cached yet.
    /// `batch_size` MUST come from `ResourcePolicy::embedding_batch_size` —
    /// never a local hardcoded constant, per Low-Level Design §4.
    pub fn new(cache_dir: &Path, batch_size: usize) -> Result<Self, SemanticError> {
        if let Some((config, tokenizer, weights, revision)) = Self::try_local_cache(cache_dir, None)
        {
            return Self::build(config, tokenizer, weights, revision, batch_size);
        }
        Self::download_and_build(cache_dir, batch_size, None)
    }

    /// Construct a `BgeEmbedder` pinned to an already-known, already-resolved
    /// commit `revision` — used when reconstructing an *already-persisted*
    /// `EmbeddingProfile` (Low-Level Design §3). [FIX] Never re-resolves
    /// "what's current" via `.info()`: reusing a persisted profile must
    /// reproduce the EXACT vector space it was created under, never a newer
    /// one that happens to have the same name — silent revision drift would
    /// be exactly the invisible-corpus-corruption failure mode invariant #4
    /// exists to prevent. Checks the local cache for this exact revision
    /// first; only touches the network if these specific files aren't
    /// already there.
    pub fn new_pinned(
        cache_dir: &Path,
        batch_size: usize,
        revision: &str,
    ) -> Result<Self, SemanticError> {
        if let Some((config, tokenizer, weights, revision)) =
            Self::try_local_cache(cache_dir, Some(revision))
        {
            return Self::build(config, tokenizer, weights, revision, batch_size);
        }
        Self::download_and_build(cache_dir, batch_size, Some(revision))
    }

    /// Check `hf-hub`'s standard on-disk cache layout directly — no network,
    /// no `hf-hub` API call. `pinned_revision`, when given, is used as-is
    /// (never re-resolved); otherwise reads the locally-cached `refs/main`
    /// pointer left by a previous real download. Returns `None` (cache miss)
    /// if any of the three needed files aren't present at that revision —
    /// the caller falls back to a real download in that case.
    fn try_local_cache(
        cache_dir: &Path,
        pinned_revision: Option<&str>,
    ) -> Option<(PathBuf, PathBuf, PathBuf, String)> {
        let repo_dir = cache_dir.join(format!("models--{HF_OWNER}--{HF_MODEL_REPO}"));
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

    /// Real network path: resolve the revision (unless `pinned_revision` is
    /// given, in which case `.info()` is skipped entirely and files are
    /// fetched directly at that revision) and download the three needed
    /// files via `hf-hub`.
    fn download_and_build(
        cache_dir: &Path,
        batch_size: usize,
        pinned_revision: Option<&str>,
    ) -> Result<Self, SemanticError> {
        let client = hf_hub::HFClient::builder()
            .cache_dir(cache_dir.to_path_buf())
            .build_sync()
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to build hf-hub client: {e}"),
            })?;
        let repo = client.model(HF_OWNER.to_string(), HF_MODEL_REPO.to_string());

        let resolved_revision = match pinned_revision {
            Some(r) => r.to_string(),
            None => {
                // Resolve the repo to an immutable commit SHA (never the
                // mutable ref "main") before hashing it into the
                // EmbeddingSpaceDescriptor. Model and tokenizer share this
                // repo, so they resolve to the same SHA.
                let info = repo
                    .info()
                    .send()
                    .map_err(|e| SemanticError::ProviderUnavailable {
                        provider: Self::PROVIDER_ID.into(),
                        reason: format!(
                            "failed to resolve {HF_OWNER}/{HF_MODEL_REPO} revision: {e}"
                        ),
                    })?;
                info.sha.ok_or_else(|| SemanticError::ProviderUnavailable {
                    provider: Self::PROVIDER_ID.into(),
                    reason: format!(
                        "{HF_OWNER}/{HF_MODEL_REPO} repo info did not include a commit sha"
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
        )
    }

    /// Shared construction path: parse `config.json`, load the safetensors
    /// weights + tokenizer, and assemble the resolved `EmbeddingSpaceDescriptor`.
    fn build(
        config_path: PathBuf,
        tokenizer_path: PathBuf,
        weights_path: PathBuf,
        resolved_revision: String,
        batch_size: usize,
    ) -> Result<Self, SemanticError> {
        let config_str = std::fs::read_to_string(&config_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to read {}: {e}", config_path.display()),
            }
        })?;
        let bert_config: BertConfig =
            serde_json::from_str(&config_str).map_err(|e| SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to parse config.json: {e}"),
            })?;
        if bert_config.hidden_size != EXPECTED_HIDDEN_SIZE {
            return Err(SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!(
                    "unexpected hidden_size {} (expected {EXPECTED_HIDDEN_SIZE}) — \
                     the downloaded config.json does not match the verified bge-base-en-v1.5 shape",
                    bert_config.hidden_size
                ),
            });
        }
        let max_tokens = bert_config
            .max_position_embeddings
            .min(EXPECTED_MAX_POSITION_EMBEDDINGS);

        let device = Device::Cpu;
        let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights_path], DTYPE, &device) }
            .map_err(|e| SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to load model weights: {e}"),
            })?;
        let model =
            BertModel::load(vb, &bert_config).map_err(|e| SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to construct BertModel: {e}"),
            })?;

        let mut tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            SemanticError::ProviderUnavailable {
                provider: Self::PROVIDER_ID.into(),
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
                provider: Self::PROVIDER_ID.into(),
                reason: format!("failed to configure tokenizer truncation: {e}"),
            })?;

        let descriptor = EmbeddingSpaceDescriptor {
            schema_version: EmbeddingSpaceDescriptor::SCHEMA_VERSION,
            provider: Self::PROVIDER_ID.into(),
            model: Self::MODEL_NAME.into(),
            model_revision: resolved_revision.clone(),
            tokenizer_revision: resolved_revision,
            pooling: PoolingStrategy::Cls,
            normalize: true,
            truncation: TruncationPolicy::Truncate,
            max_tokens,
        };

        Ok(Self {
            model,
            tokenizer,
            device,
            batch_size: batch_size.max(1),
            dims: bert_config.hidden_size,
            max_tokens,
            descriptor,
        })
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
                provider: Self::PROVIDER_ID.into(),
                reason: format!(
                    "failed to fetch {filename} from {HF_OWNER}/{HF_MODEL_REPO}@{revision}: {e}"
                ),
            })
    }

    /// The resolved, immutable vector-space identity for this instance —
    /// used to claim/compare a persisted `EmbeddingProfile` (Low-Level
    /// Design §3). Never a mutable ref like `"main"`.
    pub fn descriptor(&self) -> &EmbeddingSpaceDescriptor {
        &self.descriptor
    }

    /// CLS-pool (index 0 of the sequence dimension) + L2-normalize one
    /// forward pass's output. `hidden_states` is `(batch, seq, hidden)`.
    fn cls_pool_and_normalize(hidden_states: &Tensor) -> candle_core::Result<Tensor> {
        let cls = hidden_states.i((.., 0, ..))?; // (batch, hidden)
        let norm = cls.sqr()?.sum_keepdim(1)?.sqrt()?;
        cls.broadcast_div(&norm)
    }

    /// Run one sub-batch through tokenize → forward → CLS-pool → normalize.
    fn embed_sub_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, SemanticError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tokenization failed: {e}")))?;

        let token_ids: Vec<Vec<u32>> = encodings.iter().map(|e| e.get_ids().to_vec()).collect();
        let attention_mask: Vec<Vec<u32>> = encodings
            .iter()
            .map(|e| e.get_attention_mask().to_vec())
            .collect();
        let seq_len = token_ids.first().map(Vec::len).unwrap_or(0);

        let token_ids = Tensor::new(token_ids, &self.device)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor build failed: {e}")))?;
        let attention_mask = Tensor::new(attention_mask, &self.device)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor build failed: {e}")))?;
        let token_type_ids = Tensor::zeros((texts.len(), seq_len), DType::U32, &self.device)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("tensor build failed: {e}")))?;

        let hidden_states = self
            .model
            .forward(&token_ids, &token_type_ids, Some(&attention_mask))
            .map_err(|e| {
                SemanticError::EmbeddingFailed(format!("BERT forward pass failed: {e}"))
            })?;
        let pooled = Self::cls_pool_and_normalize(&hidden_states)
            .map_err(|e| {
                SemanticError::EmbeddingFailed(format!("pooling/normalization failed: {e}"))
            })?
            .to_dtype(DType::F32)
            .map_err(|e| SemanticError::EmbeddingFailed(format!("dtype conversion failed: {e}")))?;
        pooled
            .to_vec2()
            .map_err(|e| SemanticError::EmbeddingFailed(format!("vector extraction failed: {e}")))
    }
}

impl SemanticProvider for BgeEmbedder {
    fn id(&self) -> &'static str {
        Self::PROVIDER_ID
    }

    fn model_id(&self) -> &str {
        Self::MODEL_NAME
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn max_input_bytes(&self) -> usize {
        // Real truncation happens at the tokenizer level (`max_tokens`); this
        // is a generous sanity cap to reject only pathologically large inputs
        // (e.g. an accidental multi-MB blob) before they ever reach
        // tokenization — NOT a precise byte/token estimate. That ratio varies
        // far too much by content to use a tight multiplier: an earlier
        // version used 8 bytes/token and rejected legitimate >512-token
        // natural-language text (repeated short words can need well over
        // 1000 bytes per 100 tokens) before the tokenizer ever got a chance
        // to truncate it — caught by `bge_reference_compat.rs`'s deliberately
        // long fixture sentence. 64 bytes/token stays a real sanity cap while
        // comfortably admitting normal long inputs through to real truncation.
        self.max_tokens * 64
    }

    fn available(&self) -> bool {
        // Construction (`new`) already loaded the model into memory; a
        // BgeEmbedder value only ever exists once that succeeded.
        true
    }

    fn embedding_descriptor(&self) -> Option<EmbeddingSpaceDescriptor> {
        Some(self.descriptor.clone())
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let t0 = Instant::now();
        let mut out = Vec::with_capacity(inputs.len());

        for chunk in inputs.chunks(self.batch_size) {
            if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SemanticError::Cancelled {
                    completed: out.len(),
                    total: inputs.len(),
                });
            }
            for item in chunk {
                if item.text.len() > self.max_input_bytes() {
                    return Err(SemanticError::InputTooLarge {
                        len: item.text.len(),
                        max: self.max_input_bytes(),
                    });
                }
            }
            let texts: Vec<&str> = chunk.iter().map(|i| i.text.as_str()).collect();
            let vectors = self.embed_sub_batch(&texts)?;
            for (item, vector) in chunk.iter().zip(vectors) {
                if vector.len() != self.dims || vector.iter().any(|v| !v.is_finite()) {
                    return Err(SemanticError::EmbeddingFailed(format!(
                        "provider produced an invalid vector for unit '{}' (len={}, expected={})",
                        item.unit_key,
                        vector.len(),
                        self.dims
                    )));
                }
                out.push(EmbeddingOutput {
                    unit_key: item.unit_key.clone(),
                    vector,
                });
            }
        }

        usage.items_embedded += out.len() as u64;
        usage.input_bytes += inputs.iter().map(|i| i.text.len() as u64).sum::<u64>();
        usage.elapsed_ms += t0.elapsed().as_millis() as u64;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // try_local_cache — pure filesystem logic, no network needed to test
    // -----------------------------------------------------------------------

    fn write_fake_cache_files(repo_dir: &Path, revision: &str) {
        let snapshot = repo_dir.join("snapshots").join(revision);
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("config.json"), "{}").unwrap();
        std::fs::write(snapshot.join("tokenizer.json"), "{}").unwrap();
        std::fs::write(snapshot.join("model.safetensors"), "fake").unwrap();
    }

    #[test]
    fn try_local_cache_misses_on_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(BgeEmbedder::try_local_cache(tmp.path(), None).is_none());
        assert!(BgeEmbedder::try_local_cache(tmp.path(), Some("some-sha")).is_none());
    }

    #[test]
    fn try_local_cache_finds_files_via_refs_main_when_unpinned() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp
            .path()
            .join(format!("models--{HF_OWNER}--{HF_MODEL_REPO}"));
        write_fake_cache_files(&repo_dir, "abc123");
        std::fs::create_dir_all(repo_dir.join("refs")).unwrap();
        std::fs::write(repo_dir.join("refs").join("main"), "abc123\n").unwrap();

        let (_, _, _, revision) = BgeEmbedder::try_local_cache(tmp.path(), None)
            .expect("should find the cached files via refs/main");
        assert_eq!(revision, "abc123");
    }

    #[test]
    fn try_local_cache_pinned_ignores_refs_main() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp
            .path()
            .join(format!("models--{HF_OWNER}--{HF_MODEL_REPO}"));
        // refs/main points at a DIFFERENT (newer) revision than the one we pin.
        std::fs::create_dir_all(repo_dir.join("refs")).unwrap();
        std::fs::write(repo_dir.join("refs").join("main"), "newer-revision\n").unwrap();
        write_fake_cache_files(&repo_dir, "pinned-revision");

        let (_, _, _, revision) = BgeEmbedder::try_local_cache(tmp.path(), Some("pinned-revision"))
            .expect("should find the cached files at the pinned revision, ignoring refs/main");
        assert_eq!(revision, "pinned-revision");
    }

    #[test]
    fn try_local_cache_misses_when_pinned_revision_not_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp
            .path()
            .join(format!("models--{HF_OWNER}--{HF_MODEL_REPO}"));
        write_fake_cache_files(&repo_dir, "some-other-revision");
        assert!(BgeEmbedder::try_local_cache(tmp.path(), Some("not-cached-revision")).is_none());
    }

    /// Network-touching on a cold `cache_dir` (first run only — `hf-hub`
    /// reuses the cache on every subsequent run); gated behind an explicit
    /// opt-in env var so `cargo test --workspace` stays fully offline by
    /// default, matching the existing pattern for every other semantic test
    /// in this crate.
    fn should_run_network_tests() -> bool {
        std::env::var("ATTIC_TEST_BGE_NETWORK").as_deref() == Ok("1")
    }

    #[test]
    fn embeds_and_normalizes_real_text() {
        if !should_run_network_tests() {
            eprintln!(
                "skipping: set ATTIC_TEST_BGE_NETWORK=1 to run (downloads/reuses a cached ~130MB model)"
            );
            return;
        }
        // Stable temp-dir cache path (not a fresh tempdir per run) so repeated
        // local test runs reuse the same `hf-hub` cache instead of
        // re-downloading — the model is fetched once per machine, not once
        // per test invocation.
        let cache_dir = std::env::temp_dir().join("attic-bge-test-cache");
        let embedder = BgeEmbedder::new(&cache_dir, 8).expect("BgeEmbedder should construct");
        assert_eq!(embedder.dimensions(), EXPECTED_HIDDEN_SIZE);

        let inputs = vec![
            EmbeddingInput {
                unit_key: "a".into(),
                text: "fn main() { println!(\"hello\"); }".into(),
            },
            EmbeddingInput {
                unit_key: "b".into(),
                text: "employer name field goes empty after clicking continue".into(),
            },
        ];
        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        let outputs = embedder
            .embed_batch(&inputs, &cancel, &mut usage, None)
            .expect("embedding should succeed");
        assert_eq!(outputs.len(), 2);
        for o in &outputs {
            assert_eq!(o.vector.len(), EXPECTED_HIDDEN_SIZE);
            let norm: f32 = o.vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-3,
                "vector should be L2-normalized, got norm={norm}"
            );
        }
        assert_eq!(usage.items_embedded, 2);
    }
}
