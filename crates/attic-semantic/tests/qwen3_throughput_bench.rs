//! Real-model CPU throughput + batch-equivalence benchmark for `Qwen3Embedder`.
//!
//! Env-gated (`ATTIC_BENCH_QWEN=1`) and `#[ignore]`d: it needs the locally
//! cached `Qwen/Qwen3-Embedding-0.6B` weights and takes minutes on CPU. It
//! never downloads anything (`from_local_cache`).
//!
//! ```text
//! $env:ATTIC_BENCH_QWEN='1'
//! cargo test --release -p attic-semantic --test qwen3_throughput_bench -- --ignored --nocapture
//! ```
//!
//! Tunables: `ATTIC_BENCH_QWEN_ITEMS` (default 48), `ATTIC_BENCH_QWEN_BATCH`
//! (default 16), `ATTIC_BENCH_QWEN_CHUNK_BYTES` (default 1600),
//! `ATTIC_BENCH_QWEN_CORPUS` (a directory of real documents to sample chunks
//! from, read-only; default: this workspace's own Rust sources).
//!
//! Reports real (unpadded) tokens/second for batched document embedding and
//! the minimum cosine between each batched vector and the same text embedded
//! alone — batching is a memory/throughput detail and must never change a
//! vector.

use std::path::{Path, PathBuf};
use std::time::Instant;

use attic_semantic::provider::{CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider};
use attic_semantic::qwen3_provider::{Qwen3Embedder, QwenPooling};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn resolve_cache_dir() -> PathBuf {
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        return PathBuf::from(hf_home).join("hub");
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".cache")
        .join("huggingface")
        .join("hub")
}

/// Deterministic, representative corpus split on line boundaries into
/// ~`chunk_bytes` chunks: files under `ATTIC_BENCH_QWEN_CORPUS` when set
/// (text-like extensions only), otherwise this workspace's own Rust sources.
/// Lines longer than a chunk (minified JSON) are split on char boundaries.
fn corpus(items: usize, chunk_bytes: usize) -> Vec<EmbeddingInput> {
    let (root, exts): (PathBuf, &[&str]) = match std::env::var("ATTIC_BENCH_QWEN_CORPUS") {
        Ok(dir) => (
            PathBuf::from(dir),
            &[
                "md", "txt", "json", "xml", "html", "java", "js", "ts", "rs", "py",
            ],
        ),
        Err(_) => (Path::new(env!("CARGO_MANIFEST_DIR")).join(".."), &["rs"]),
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().is_some_and(|n| n != "target") {
                    stack.push(p);
                }
            } else if p
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| exts.contains(&x.to_ascii_lowercase().as_str()))
            {
                files.push(p);
            }
        }
    }
    files.sort();
    // Spread the sample across files so one multi-megabyte export cannot
    // make up the whole corpus.
    let per_file = items.div_ceil(files.len().max(1)).max(1);
    let mut out = Vec::with_capacity(items);
    let push = |out: &mut Vec<EmbeddingInput>, text: String| {
        if !text.trim().is_empty() {
            out.push(EmbeddingInput {
                unit_key: format!("u{}", out.len()),
                text,
            });
        }
    };
    'files: for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let file_start = out.len();
        let mut chunk = String::new();
        'lines: for line in text.lines() {
            let mut rest = line;
            while !rest.is_empty() {
                let room = chunk_bytes.saturating_sub(chunk.len()).max(1);
                let mut take = rest.len().min(room);
                while !rest.is_char_boundary(take) {
                    take -= 1;
                }
                if take == 0 {
                    take = rest.chars().next().map_or(rest.len(), char::len_utf8);
                }
                chunk.push_str(&rest[..take]);
                rest = &rest[take..];
                if chunk.len() >= chunk_bytes {
                    push(&mut out, std::mem::take(&mut chunk));
                    if out.len() == items {
                        break 'files;
                    }
                    if out.len() - file_start >= per_file {
                        break 'lines;
                    }
                }
            }
            chunk.push('\n');
        }
        if !chunk.is_empty() && out.len() - file_start < per_file {
            push(&mut out, std::mem::take(&mut chunk));
            if out.len() == items {
                break 'files;
            }
        }
    }
    out
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

#[test]
#[ignore = "real Qwen3 CPU benchmark; set ATTIC_BENCH_QWEN=1 and run with --ignored"]
fn qwen3_cpu_throughput_and_batch_equivalence() {
    if std::env::var("ATTIC_BENCH_QWEN").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_BENCH_QWEN!=1; skipping");
        return;
    }
    let items = env_usize("ATTIC_BENCH_QWEN_ITEMS", 48);
    let batch = env_usize("ATTIC_BENCH_QWEN_BATCH", 16);
    let chunk_bytes = env_usize("ATTIC_BENCH_QWEN_CHUNK_BYTES", 1600);

    let load_start = Instant::now();
    let embedder =
        Qwen3Embedder::from_local_cache(&resolve_cache_dir(), batch, None, QwenPooling::LastToken)
            .expect("Qwen3 weights must be present in the local HF cache for this benchmark");
    let load_ms = load_start.elapsed().as_millis();

    let inputs = corpus(items, chunk_bytes);
    assert!(!inputs.is_empty(), "benchmark corpus is empty");
    let real_tokens: usize = inputs
        .iter()
        .map(|i| {
            embedder
                .tokenizer()
                .encode(i.text.as_str(), true)
                .map(|e| e.get_ids().len())
                .unwrap_or(0)
        })
        .sum();

    // Warm-up: first forward pays one-off allocation/page-in costs.
    let cancel = CancelFlag::new();
    let mut usage = ResourceUsage::default();
    embedder
        .embed_batch(&inputs[..1], &cancel, &mut usage, None)
        .expect("warm-up embed");

    let t0 = Instant::now();
    let batched = embedder
        .embed_batch(&inputs, &cancel, &mut usage, None)
        .expect("batched embed");
    let batched_s = t0.elapsed().as_secs_f64();
    assert_eq!(batched.len(), inputs.len());

    // Batch equivalence on a sample (every item embedded alone is expensive).
    let sample = inputs.len().min(8);
    let mut min_cos = f32::INFINITY;
    for (input, out) in inputs.iter().zip(&batched).take(sample) {
        let single = embedder
            .embed_batch(std::slice::from_ref(input), &cancel, &mut usage, None)
            .expect("single embed");
        min_cos = min_cos.min(cosine(&single[0].vector, &out.vector));
    }

    eprintln!(
        "QWEN3_BENCH items={} batch={} chunk_bytes={} real_tokens={} load_ms={} \
         batched_s={:.2} tok_per_s={:.1} items_per_s={:.2} min_batch_vs_single_cosine={:.6} \
         rayon_threads={}",
        inputs.len(),
        batch,
        chunk_bytes,
        real_tokens,
        load_ms,
        batched_s,
        real_tokens as f64 / batched_s,
        inputs.len() as f64 / batched_s,
        min_cos,
        rayon_threads(),
    );
    assert!(
        min_cos >= 0.9999,
        "batched vectors must match single-item vectors (min cosine {min_cos})"
    );
}

fn rayon_threads() -> String {
    std::env::var("RAYON_NUM_THREADS").unwrap_or_else(|_| "default".into())
}
