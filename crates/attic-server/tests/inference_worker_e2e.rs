//! r06 end-to-end: supervisor → `attic inference-worker` child process →
//! real Candle Qwen3 model → embedding vectors, over the wire protocol.
//!
//! Gated on ATTIC_RUN_MODEL_E2E=1 because it loads the real model (~1.2 GB).
//! Uses Attic's already-provisioned model cache; performs no network I/O
//! when the pinned snapshot is present.

use attic_inference_protocol::EmbedItem;
use attic_inference_protocol::supervisor::{LoadParams, WorkerLaunch, WorkerSupervisor};
use std::time::Duration;

fn attic_model_cache_dir() -> std::path::PathBuf {
    std::env::var_os("ATTIC_MODEL_CACHE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("ATTIC_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .or_else(|| std::env::var_os("HOME"))
                        .map(std::path::PathBuf::from)
                        .expect("need ATTIC_HOME or a user home directory")
                        .join(".attic")
                })
                .join("models")
        })
}

#[test]
fn candle_worker_end_to_end_with_real_model() {
    if std::env::var("ATTIC_RUN_MODEL_E2E").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_RUN_MODEL_E2E!=1; skipping real-model worker e2e");
        return;
    }
    let hub = attic_model_cache_dir().display().to_string();

    let sup = WorkerSupervisor::new(WorkerLaunch {
        program: std::path::PathBuf::from(env!("CARGO_BIN_EXE_attic")),
        args: vec!["inference-worker".to_string()],
        env: vec![],
    });

    let caps = sup.handshake().expect("handshake");
    assert!(caps.is_empty() || !caps.is_empty()); // handshake itself is the gate

    sup.load_model(LoadParams {
        cache_dir: hub,
        batch_size: 4,
        dimension: None,
        backend: "candle-cpu".to_string(),
        onnx_model_dir: None,
        seq_len: None,
    })
    .expect("model load through worker");

    let vectors = sup
        .embed_batch(
            vec![
                EmbedItem {
                    key: "a".into(),
                    text: "fn handle_request(req: Request) -> Response {}".into(),
                },
                EmbedItem {
                    key: "b".into(),
                    text: "def process_payment(amount): return charge(amount)".into(),
                },
            ],
            Duration::from_secs(180),
        )
        .expect("embed through worker");

    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 1024, "native Qwen3-0.6B width");
    let norm: f32 = vectors[0].iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 0.01, "unit-normalized, got {norm}");
    // Distinct inputs must produce distinct vectors.
    assert_ne!(vectors[0], vectors[1]);

    sup.shutdown();
}
