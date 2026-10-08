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

/// How many times a unit that overflows the model's TOKEN window (dense text
/// packs more than the assumed tokens per byte) is re-split at half the byte
/// window. Two halvings take a 1 KiB window to 256 bytes, which no byte-level
/// BPE tokenizer can turn into more than ~257 tokens.
const MAX_TOKEN_RESPLITS: u32 = 2;

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
        if window == 0 {
            return self.inner.embed_batch(inputs, cancel, usage, deadline);
        }
        let cap = windowed_capacity(window);
        if let Some(big) = inputs.iter().find(|i| i.text.len() > cap) {
            return Err(SemanticError::InputTooLarge {
                len: big.text.len(),
                max: cap,
            });
        }
        self.embed_units(inputs, window, 0, cancel, usage, deadline)
    }
}

impl WindowedProvider<'_> {
    /// Embed `inputs` with byte windows of `window`. The byte window is only
    /// an estimate of the model's token window: dense text (symbols, non-Latin
    /// scripts) can exceed the token budget inside it. On that error the batch
    /// is bisected down to the offending unit, and only that unit is re-split
    /// at half the window — every other unit keeps its normal vector.
    fn embed_units(
        &self,
        inputs: &[EmbeddingInput],
        window: usize,
        resplits: u32,
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        match self.embed_split(inputs, window, cancel, usage, deadline) {
            Err(SemanticError::InputTooManyTokens { .. }) if inputs.len() > 1 => {
                let (a, b) = inputs.split_at(inputs.len() / 2);
                let mut out = self.embed_units(a, window, resplits, cancel, usage, deadline)?;
                out.extend(self.embed_units(b, window, resplits, cancel, usage, deadline)?);
                Ok(out)
            }
            Err(SemanticError::InputTooManyTokens { tokens, max })
                if resplits < MAX_TOKEN_RESPLITS && window >= 8 =>
            {
                tracing::debug!(
                    unit = %inputs[0].unit_key,
                    tokens,
                    max,
                    window = window / 2,
                    "unit exceeds the token window; re-splitting it smaller"
                );
                self.embed_units(inputs, window / 2, resplits + 1, cancel, usage, deadline)
            }
            other => other,
        }
    }

    /// One provider call: units above `window` bytes are split and pooled;
    /// units that fit are passed through and returned unchanged.
    fn embed_split(
        &self,
        inputs: &[EmbeddingInput],
        window: usize,
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        if inputs.iter().all(|i| i.text.len() <= window) {
            return self.inner.embed_batch(inputs, cancel, usage, deadline);
        }

        // Expand: every window gets a key unique WITHIN THIS CALL (input
        // position, not the caller's `unit_key`, which need not be unique —
        // a duplicate would overwrite its twin's entry and fail the batch);
        // remember which unit it belongs to, how many bytes it carries, and
        // whether it is a split. Results are re-keyed to `unit_key` below.
        let mut expanded: Vec<EmbeddingInput> = Vec::with_capacity(inputs.len() * 2);
        let mut owner: HashMap<String, (usize, usize)> = HashMap::new();
        for (idx, input) in inputs.iter().enumerate() {
            if input.text.len() <= window {
                let key = format!("{idx}\u{1f}u");
                owner.insert(key.clone(), (idx, input.text.len()));
                expanded.push(EmbeddingInput {
                    unit_key: key,
                    text: input.text.clone(),
                });
                continue;
            }
            for (w, piece) in split_windows(&input.text, window).into_iter().enumerate() {
                let key = format!("{idx}\u{1f}w{w}");
                owner.insert(key.clone(), (idx, piece.len()));
                expanded.push(EmbeddingInput {
                    unit_key: key,
                    text: piece.to_string(),
                });
            }
        }

        let outputs = self.inner.embed_batch(&expanded, cancel, usage, deadline)?;

        // Pool: length-weighted mean per unit, then L2-normalize. A unit
        // that was not split has exactly one vector and is returned as-is.
        let mut sums: Vec<Option<Vec<f32>>> = vec![None; inputs.len()];
        for out in outputs {
            let Some(&(idx, bytes)) = owner.get(&out.unit_key) else {
                continue;
            };
            let weight = bytes.max(1) as f32;
            match &mut sums[idx] {
                None => {
                    let v = if inputs[idx].text.len() <= window {
                        out.vector
                    } else {
                        out.vector.iter().map(|x| x * weight).collect()
                    };
                    sums[idx] = Some(v);
                }
                Some(acc) => {
                    if acc.len() != out.vector.len() {
                        return Err(SemanticError::DimensionMismatch {
                            record: out.vector.len(),
                            expected: acc.len(),
                        });
                    }
                    for (a, v) in acc.iter_mut().zip(&out.vector) {
                        *a += v * weight;
                    }
                }
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
            if inputs[idx].text.len() > window {
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    v.iter_mut().for_each(|x| *x /= norm);
                }
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
    fn duplicate_unit_keys_in_a_split_batch_each_get_a_vector() {
        let p = WindowedProvider::new(&Echo);
        let out = p
            .embed_batch(
                &[
                    input("dup", "aaa"),
                    input("dup", "bbb"),
                    input("big", "aaaaaaaabbbbbbbb"),
                ],
                &CancelFlag::new(),
                &mut ResourceUsage::default(),
                None,
            )
            .expect("duplicate keys must not fail the batch");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].unit_key, "dup");
        assert_eq!(out[1].unit_key, "dup");
        assert_eq!(out[2].unit_key, "big");
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

    /// Rejects any item with more than two 'z' (or any '!') — standing in
    /// for text whose token count exceeds the model window even though its
    /// bytes fit.
    struct Dense;
    impl SemanticProvider for Dense {
        fn id(&self) -> &'static str {
            "dense"
        }
        fn model_id(&self) -> &str {
            "dense"
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
            c: &CancelFlag,
            u: &mut ResourceUsage,
            d: Option<std::time::Instant>,
        ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
            if let Some(i) = inputs
                .iter()
                .find(|i| i.text.matches('z').count() > 2 || i.text.contains('!'))
            {
                return Err(SemanticError::InputTooManyTokens {
                    tokens: i.text.len() * 2,
                    max: 8,
                });
            }
            Echo.embed_batch(inputs, c, u, d)
        }
    }

    #[test]
    fn token_overflow_resplits_only_the_offending_unit() {
        let p = WindowedProvider::new(&Dense);
        let out = p
            .embed_batch(
                &[
                    input("good", "aaa"),
                    input("dense", "zzaazzaa"),
                    input("also-good", "bbbb"),
                ],
                &CancelFlag::new(),
                &mut ResourceUsage::default(),
                None,
            )
            .expect("dense unit must be re-split, not failed");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].unit_key, "good");
        assert_eq!(
            out[0].vector,
            vec![1.0, 0.0],
            "untouched units keep their vector"
        );
        assert_eq!(out[2].vector, vec![0.0, 1.0]);
        let n = (out[1].vector[0].powi(2) + out[1].vector[1].powi(2)).sqrt();
        assert!((n - 1.0).abs() < 1e-5);
    }

    #[test]
    fn token_overflow_that_never_fits_is_reported_not_looped() {
        let p = WindowedProvider::new(&Dense);
        // '!' overflows at every window size, so the bounded re-splits run
        // out and the error is reported instead of recursing forever.
        let err = p
            .embed_batch(
                &[input("dense", "!!!!!!!!")],
                &CancelFlag::new(),
                &mut ResourceUsage::default(),
                None,
            )
            .unwrap_err();
        assert!(matches!(err, SemanticError::InputTooManyTokens { .. }));
    }
}
