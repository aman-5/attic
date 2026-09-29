//! Windowed embedding for units larger than the model's input window.
//!
//! A provider reads at most `max_input_bytes()` per item (e.g. 1 KiB for
//! DirectML at seq_len 512, 2 KiB for Candle). Units above that used to be
//! excluded from embedding outright, so on a prose-heavy corpus almost every
//! documentation chunk was silently lexical-only. [`WindowedProvider`] instead
//! splits such a unit into consecutive windows that each fit the model,
//! embeds every window, and returns ONE vector per unit: the length-weighted
//! mean of the window vectors, re-normalized. Units that already fit are
//! passed through untouched, so their vectors are bit-identical to before and
//! no vector-space fingerprint changes.
//!
//! Nothing is truncated: a unit larger than [`MAX_WINDOWS_PER_UNIT`] windows
//! is still excluded at selection (counted as `exceeds_max_input_bytes`).

use std::collections::HashMap;

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ProviderConcurrencyContract,
    ResourceUsage, SemanticProvider, WorkerStatus,
};

/// Upper bound on windows per unit, bounding the cost of one huge unit.
pub const MAX_WINDOWS_PER_UNIT: usize = 16;

/// Largest unit the windowed path accepts for a provider window of `window`.
pub const fn windowed_capacity(window: usize) -> usize {
    window.saturating_mul(MAX_WINDOWS_PER_UNIT)
}

/// Split `text` into consecutive pieces of at most `max` bytes, on char
/// boundaries, preferring a line break in the last quarter of each window.
pub fn split_windows(text: &str, max: usize) -> Vec<&str> {
    let max = max.max(4);
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if let Some(nl) = rest[..cut].rfind('\n')
            && nl + 1 >= cut - cut / 4
        {
            cut = nl + 1;
        }
        out.push(&rest[..cut]);
        rest = &rest[cut..];
    }
    if !rest.is_empty() || out.is_empty() {
        out.push(rest);
    }
    out
}

/// Wraps a provider so oversized units are embedded window-by-window and
/// pooled into a single vector. Everything else delegates unchanged.
pub struct WindowedProvider<'a> {
    inner: &'a dyn SemanticProvider,
}

impl<'a> WindowedProvider<'a> {
    pub fn new(inner: &'a dyn SemanticProvider) -> Self {
        Self { inner }
    }
}

impl SemanticProvider for WindowedProvider<'_> {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_input_bytes(&self) -> usize {
        windowed_capacity(self.inner.max_input_bytes())
    }
    fn available(&self) -> bool {
        self.inner.available()
    }
    fn model_lifecycle(&self) -> Option<String> {
        self.inner.model_lifecycle()
    }
    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        self.inner.concurrency_contract()
    }
    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        self.inner.fingerprint()
    }
    fn fallback_reason(&self) -> Option<String> {
        self.inner.fallback_reason()
    }
    fn preferred_claim_items(&self) -> Option<usize> {
        self.inner.preferred_claim_items()
    }
    fn worker_status(&self) -> Option<WorkerStatus> {
        self.inner.worker_status()
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let window = self.inner.max_input_bytes();
        if window == 0 || inputs.iter().all(|i| i.text.len() <= window) {
            return self.inner.embed_batch(inputs, cancel, usage, deadline);
        }

        // Expand: every window gets a unique key; remember which unit and
        // how many bytes it carries.
        let mut expanded: Vec<EmbeddingInput> = Vec::with_capacity(inputs.len() * 2);
        let mut owner: HashMap<String, (usize, usize)> = HashMap::new();
        for (idx, input) in inputs.iter().enumerate() {
            if input.text.len() > windowed_capacity(window) {
                return Err(SemanticError::InputTooLarge {
                    len: input.text.len(),
                    max: windowed_capacity(window),
                });
            }
            if input.text.len() <= window {
                owner.insert(input.unit_key.clone(), (idx, input.text.len()));
                expanded.push(input.clone());
                continue;
            }
            for (w, piece) in split_windows(&input.text, window).into_iter().enumerate() {
                let key = format!("{}\u{1f}w{w}", input.unit_key);
                owner.insert(key.clone(), (idx, piece.len()));
                expanded.push(EmbeddingInput {
                    unit_key: key,
                    text: piece.to_string(),
                });
            }
        }

        let outputs = self.inner.embed_batch(&expanded, cancel, usage, deadline)?;

        // Pool: length-weighted mean per unit, then L2-normalize.
        let dims = self.inner.dimensions();
        let mut sums: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        for out in outputs {
            let Some(&(idx, bytes)) = owner.get(&out.unit_key) else {
                continue;
            };
            let weight = bytes.max(1) as f32;
            let acc = sums[idx].get_or_insert_with(|| vec![0.0; out.vector.len().max(dims)]);
            for (a, v) in acc.iter_mut().zip(&out.vector) {
                *a += v * weight;
            }
        }
        let mut pooled = Vec::with_capacity(inputs.len());
        for (idx, sum) in sums.into_iter().enumerate() {
            let Some(mut v) = sum else {
                return Err(SemanticError::EmbeddingFailed(format!(
                    "no window embeddings returned for unit '{}'",
                    inputs[idx].unit_key
                )));
            };
            let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                v.iter_mut().for_each(|x| *x /= norm);
            }
            pooled.push(EmbeddingOutput {
                unit_key: inputs[idx].unit_key.clone(),
                vector: v,
            });
        }
        Ok(pooled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl SemanticProvider for Echo {
        fn id(&self) -> &'static str {
            "echo"
        }
        fn model_id(&self) -> &str {
            "echo"
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn max_input_bytes(&self) -> usize {
            8
        }
        fn embed_batch(
            &self,
            inputs: &[EmbeddingInput],
            _c: &CancelFlag,
            _u: &mut ResourceUsage,
            _d: Option<std::time::Instant>,
        ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
            inputs
                .iter()
                .map(|i| {
                    if i.text.len() > 8 {
                        return Err(SemanticError::InputTooLarge {
                            len: i.text.len(),
                            max: 8,
                        });
                    }
                    // 'a'-heavy text points along x, anything else along y.
                    let a = i.text.matches('a').count() as f32;
                    let v = if a > 0.0 {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    };
                    Ok(EmbeddingOutput {
                        unit_key: i.unit_key.clone(),
                        vector: v,
                    })
                })
                .collect()
        }
    }

    fn input(k: &str, t: &str) -> EmbeddingInput {
        EmbeddingInput {
            unit_key: k.into(),
            text: t.into(),
        }
    }

    #[test]
    fn split_respects_limit_char_boundaries_and_covers_everything() {
        let text = "héllo wörld\nsecond line\nthird ✓ line";
        for max in [4, 5, 7, 12] {
            let parts = split_windows(text, max);
            assert!(parts.iter().all(|p| p.len() <= max.max(4)));
            assert_eq!(parts.concat(), text);
        }
        assert_eq!(split_windows("", 8), vec![""]);
    }

    #[test]
    fn oversized_units_get_one_pooled_normalized_vector_small_ones_pass_through() {
        let p = WindowedProvider::new(&Echo);
        assert_eq!(p.max_input_bytes(), 8 * MAX_WINDOWS_PER_UNIT);
        let out = p
            .embed_batch(
                &[input("small", "aaa"), input("big", "aaaaaaaabbbbbbbb")],
                &CancelFlag::new(),
                &mut ResourceUsage::default(),
                None,
            )
            .expect("embed");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].unit_key, "small");
        assert_eq!(out[0].vector, vec![1.0, 0.0]);
        assert_eq!(out[1].unit_key, "big");
        let n = (out[1].vector[0].powi(2) + out[1].vector[1].powi(2)).sqrt();
        assert!((n - 1.0).abs() < 1e-5, "pooled vector must be unit length");
        assert!(out[1].vector[0] > 0.0 && out[1].vector[1] > 0.0);
    }

    #[test]
    fn units_beyond_the_window_budget_are_rejected_not_truncated() {
        let p = WindowedProvider::new(&Echo);
        let huge = "x".repeat(8 * MAX_WINDOWS_PER_UNIT + 1);
        let err = p
            .embed_batch(
                &[input("huge", &huge)],
                &CancelFlag::new(),
                &mut ResourceUsage::default(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, SemanticError::InputTooLarge { .. }));
    }
}
