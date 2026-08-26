//! Built-in deterministic embedder plus test doubles (ADR-013).
//!
//! `HashingEmbedder` is the V1 DEFAULT provider: pure Rust, zero model
//! downloads, fully offline, byte-deterministic — which is what makes the
//! Phase 5 benchmark gate and all §22 tests reproducible. It hashes word
//! unigrams + character trigrams into a fixed 256-dimensional bag-of-
//! features space with L2 normalization (a standard "feature hashing"
//! baseline; see ADR-013 for why a neural provider is optional, not
//! required, for V1).

use std::time::Instant;

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingInput, EmbeddingOutput, ResourceUsage, SemanticProvider,
};

/// Deterministic feature-hashing embedder ("hashing", model "hashed-ngram-v1").
#[derive(Debug)]
pub struct HashingEmbedder {
    dims: usize,
    max_input_bytes: usize,
}

impl HashingEmbedder {
    pub const ID: &'static str = "hashing";
    pub const MODEL: &'static str = "hashed-ngram-v1";

    pub fn new() -> Self {
        Self {
            dims: 256,
            max_input_bytes: 16_384,
        }
    }

    pub fn with_dims(dims: usize) -> Self {
        Self {
            dims,
            max_input_bytes: 16_384,
        }
    }

    /// Deterministic feature bucket for one token.
    fn bucket(&self, token: &str) -> usize {
        let h = blake3::hash(token.as_bytes());
        let v = u64::from_le_bytes(h.as_bytes()[0..8].try_into().unwrap());
        (v % self.dims as u64) as usize
    }

    /// Tokenize + emit features (word unigrams and char trigrams).
    fn features(&self, text: &str) -> Vec<String> {
        let lower = text.to_lowercase();
        let mut feats = Vec::new();
        for tok in lower.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let tok = tok.trim_matches('_');
            if tok.is_empty() {
                continue;
            }
            feats.push(format!("w:{tok}"));
            if tok.len() >= 3 {
                let chars: Vec<char> = tok.chars().collect();
                if chars.len() >= 3 {
                    for w in chars.windows(3) {
                        feats.push(format!("t:{}", w.iter().collect::<String>()));
                    }
                }
            }
        }
        feats
    }

    fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0.0f32; self.dims];
        for f in self.features(text) {
            v[self.bucket(&f)] += 1.0;
        }
        let norm = v
            .iter()
            .map(|x| (*x as f64) * (*x as f64))
            .sum::<f64>()
            .sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm as f32;
            }
        }
        v
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticProvider for HashingEmbedder {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn model_id(&self) -> &str {
        Self::MODEL
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    fn available(&self) -> bool {
        true // no external resources; always available
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
        for input in inputs {
            if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SemanticError::Cancelled {
                    completed: out.len(),
                    total: inputs.len(),
                });
            }
            if input.text.len() > self.max_input_bytes {
                return Err(SemanticError::InputTooLarge {
                    len: input.text.len(),
                    max: self.max_input_bytes,
                });
            }
            out.push(EmbeddingOutput {
                unit_key: input.unit_key.clone(),
                vector: self.embed_one(&input.text),
            });
        }
        usage.items_embedded += out.len() as u64;
        usage.input_bytes += inputs.iter().map(|i| i.text.len() as u64).sum::<u64>();
        usage.elapsed_ms += t0.elapsed().as_millis() as u64;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Test doubles (used across §22 suites)
// ---------------------------------------------------------------------------

/// Provider that reports unavailable (models missing / endpoint down).
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

/// Provider whose generation always fails after `fail_after` items.
pub struct FailingProvider {
    pub fail_after: usize,
}

impl SemanticProvider for FailingProvider {
    fn id(&self) -> &'static str {
        "failing"
    }
    fn model_id(&self) -> &str {
        "failing-v1"
    }
    fn dimensions(&self) -> usize {
        4
    }
    fn max_input_bytes(&self) -> usize {
        4096
    }
    fn available(&self) -> bool {
        true
    }
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        _cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        _deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let mut out = Vec::new();
        for (i, input) in inputs.iter().enumerate() {
            if i >= self.fail_after {
                break;
            }
            out.push(EmbeddingOutput {
                unit_key: input.unit_key.clone(),
                vector: vec![0.5; 4],
            });
        }
        usage.items_embedded += out.len() as u64;
        Err(SemanticError::EmbeddingFailed(format!(
            "injected failure with {} completed",
            out.len()
        )))
    }
}

/// Sleeps `delay_ms` before each item; honors cancellation between items so
/// bounded-drive tests can stop it deterministically.
pub struct SlowProvider {
    pub delay_ms: u64,
}

impl SemanticProvider for SlowProvider {
    fn id(&self) -> &'static str {
        "slow"
    }
    fn model_id(&self) -> &str {
        "slow-v1"
    }
    fn dimensions(&self) -> usize {
        2
    }
    fn max_input_bytes(&self) -> usize {
        4096
    }
    fn available(&self) -> bool {
        true
    }
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let mut out = Vec::new();
        for input in inputs {
            if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Err(SemanticError::Cancelled {
                    completed: out.len(),
                    total: inputs.len(),
                });
            }
            // Sleep in small slices so deadline/cancel are honored MID-delay —
            // a provider must never block past the caller's budget.
            let mut remaining = self.delay_ms;
            while remaining > 0 {
                if cancel.is_cancelled() || deadline.is_some_and(|d| Instant::now() >= d) {
                    return Err(SemanticError::Cancelled {
                        completed: out.len(),
                        total: inputs.len(),
                    });
                }
                let step = remaining.min(5);
                std::thread::sleep(std::time::Duration::from_millis(step));
                remaining -= step;
            }
            out.push(EmbeddingOutput {
                unit_key: input.unit_key.clone(),
                vector: vec![1.0, 0.0],
            });
        }
        usage.items_embedded += out.len() as u64;
        Ok(out)
    }
}

/// Returns caller-supplied vectors keyed by input order; records every text
/// it was asked to embed (security assertion: nothing secret reaches here).
pub struct RecordingProvider {
    pub vectors: Vec<Vec<f32>>,
    pub seen_texts: std::sync::Mutex<Vec<String>>,
}

impl SemanticProvider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn model_id(&self) -> &str {
        "recording-v1"
    }
    fn dimensions(&self) -> usize {
        self.vectors.first().map(Vec::len).unwrap_or(1)
    }
    fn max_input_bytes(&self) -> usize {
        4096
    }
    fn available(&self) -> bool {
        true
    }
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        _cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        _deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        self.seen_texts
            .lock()
            .unwrap()
            .extend(inputs.iter().map(|i| i.text.clone()));
        let out: Vec<EmbeddingOutput> = inputs
            .iter()
            .enumerate()
            .map(|(i, input)| EmbeddingOutput {
                unit_key: input.unit_key.clone(),
                vector: self.vectors[i % self.vectors.len()].clone(),
            })
            .collect();
        usage.items_embedded += out.len() as u64;
        Ok(out)
    }
}
