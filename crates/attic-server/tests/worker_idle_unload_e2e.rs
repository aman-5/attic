//! Idle unload end-to-end against the real `attic inference-worker` child:
//! no process at construction, loads on first embed, the process exits after
//! the idle window, and the next embed reloads and returns the same vector.
//!
//! Gated on ATTIC_RUN_MODEL_E2E=1 (loads the real model). Uses the ONNX
//! DirectML export when ATTIC_ONNX_MODEL_DIR is set and the binary was built
//! with `ort-directml`; otherwise the Candle CPU weights in the HF cache.

use std::sync::Arc;
use std::time::{Duration, Instant};

use attic_inference_protocol::supervisor::{LoadParams, WorkerLaunch};
use attic_semantic::{CancelFlag, EmbeddingInput, ResourceUsage, SemanticProvider};

fn hf_hub() -> String {
    std::env::var("HF_HOME")
        .map(|h| format!("{h}/hub"))
        .or_else(|_| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(|h| format!("{h}/.cache/huggingface/hub"))
        })
        .expect("need a Hugging Face cache location")
}

fn embed(p: &dyn SemanticProvider, text: &str) -> (Vec<f32>, ResourceUsage) {
    let mut usage = ResourceUsage::default();
    let out = p
        .embed_batch(
            &[EmbeddingInput {
                unit_key: "q".into(),
                text: text.into(),
            }],
            &CancelFlag::new(),
            &mut usage,
            Some(Instant::now() + Duration::from_secs(2)),
        )
        .expect("embed");
    (out.into_iter().next().unwrap().vector, usage)
}

#[test]
fn idle_worker_exits_and_the_next_query_reloads_it() {
    if std::env::var("ATTIC_RUN_MODEL_E2E").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_RUN_MODEL_E2E!=1; skipping idle-unload e2e");
        return;
    }
    let onnx = std::env::var("ATTIC_ONNX_MODEL_DIR").ok();
    let backend = if onnx.is_some() && cfg!(feature = "ort-directml") {
        "ort-directml"
    } else {
        "candle-cpu"
    };
    let seq_len = 512;
    let provider = Arc::new(
        attic_semantic::SupervisedWorkerProvider::new(
            WorkerLaunch {
                program: std::path::PathBuf::from(env!("CARGO_BIN_EXE_attic")),
                args: vec!["inference-worker".to_string()],
                env: vec![],
            },
            LoadParams {
                cache_dir: hf_hub(),
                batch_size: 4,
                dimension: None,
                backend: backend.to_string(),
                onnx_model_dir: (backend == "ort-directml").then(|| onnx.clone().unwrap()),
                seq_len: Some(seq_len),
            },
            attic_semantic::expected_fingerprint(backend, None),
            attic_semantic::expected_max_input_bytes(backend, seq_len),
        )
        .with_idle_unload(Duration::from_secs(3)),
    );
    provider.spawn_idle_reaper();

    assert!(provider.worker_pid().is_none(), "no worker before first use");
    assert_eq!(provider.worker_status().unwrap().state, "not_loaded");

    // A 2 s query deadline is far shorter than a cold load; the load is
    // refunded, so the cold query must still succeed.
    let (v1, u1) = embed(provider.as_ref(), "fn authenticate_user(token: &str) -> bool");
    assert!(u1.warmup_ms > 0, "first use pays the load");
    let pid1 = provider.worker_pid().expect("worker running after first use");
    let s = provider.worker_status().unwrap();
    assert_eq!(s.state, "loaded");
    eprintln!("backend={backend} first load: {}", s.detail);

    let deadline = Instant::now() + Duration::from_secs(30);
    while provider.worker_status().unwrap().state != "unloaded" {
        assert!(Instant::now() < deadline, "worker was never unloaded");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(provider.worker_pid().is_none(), "worker process stopped");
    eprintln!("after idle: {}", provider.worker_status().unwrap().detail);

    let (v2, u2) = embed(provider.as_ref(), "fn authenticate_user(token: &str) -> bool");
    assert!(u2.warmup_ms > 0, "reload after unload is a cold start");
    let pid2 = provider.worker_pid().expect("worker restarted");
    assert_ne!(pid1, pid2, "a new worker process serves after unload");
    let dot: f32 = v1.iter().zip(&v2).map(|(a, b)| a * b).sum();
    assert!(dot > 0.999, "reloaded model must produce the same vector, cos={dot}");
    eprintln!(
        "reload: {} (warmup {} ms)",
        provider.worker_status().unwrap().detail,
        u2.warmup_ms
    );
}
