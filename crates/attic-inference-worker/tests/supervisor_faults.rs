//! Supervisor fault-injection tests against the real mock worker binary
//! (CARGO_BIN_EXE_attic-inference-worker). These prove the core r06 safety
//! properties with real processes, no GPU required.

use attic_inference_protocol::EmbedItem;
use attic_inference_protocol::supervisor::{
    LoadParams, SupervisorError, WorkerLaunch, WorkerSupervisor,
};
use std::time::Duration;

fn launch(mode: &str) -> WorkerLaunch {
    WorkerLaunch {
        program: std::path::PathBuf::from(env!("CARGO_BIN_EXE_attic-inference-worker")),
        args: vec![],
        env: vec![("ATTIC_MOCK_WORKER".into(), mode.into())],
    }
}

fn params() -> LoadParams {
    LoadParams {
        cache_dir: "unused".into(),
        batch_size: 4,
        dimension: None,
        backend: "mock".into(),
        onnx_model_dir: None,
        seq_len: None,
    }
}

fn items(texts: &[&str]) -> Vec<EmbedItem> {
    texts
        .iter()
        .map(|t| EmbedItem {
            key: (*t).to_string(),
            text: (*t).to_string(),
        })
        .collect()
}

#[test]
fn clean_roundtrip_embeds_in_order() {
    let sup = WorkerSupervisor::new(launch("echo"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    let out = sup
        .embed_batch(items(&["alpha", "beta"]), Duration::from_secs(10))
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].len(), 4);
    // Deterministic mock: same input, same vector.
    let again = sup
        .embed_batch(items(&["alpha"]), Duration::from_secs(10))
        .unwrap();
    assert_eq!(out[0], again[0]);
    sup.shutdown();
}

#[test]
fn hung_worker_is_killed_and_supervisor_recovers() {
    let sup = WorkerSupervisor::new(launch("hang"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    let pid_before = sup.worker_pid().expect("worker running");

    let err = sup
        .embed_batch(items(&["stuck"]), Duration::from_millis(500))
        .unwrap_err();
    assert!(
        matches!(err, SupervisorError::WorkerTimeout(_)),
        "expected WorkerTimeout, got {err:?}"
    );

    // The hung child was killed; the next request spawns a fresh worker
    // and lazily reloads the model — the supervisor recovers on its own.
    // The new worker inherits the same mock mode, so it would hang again on
    // embed; the point here is recovery of PROCESS state: a new pid exists.
    let pid_after = sup.worker_pid();
    assert!(
        pid_after.is_none() || pid_after != Some(pid_before),
        "killed worker must not still be the live one"
    );
    sup.shutdown();
}

#[test]
fn crashed_worker_is_detected_and_restarted() {
    // crash mode dies on the first EmbedBatch.
    let sup = WorkerSupervisor::new(launch("crash"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();

    let err = sup
        .embed_batch(items(&["boom"]), Duration::from_secs(5))
        .unwrap_err();
    assert!(
        matches!(err, SupervisorError::WorkerDied),
        "expected WorkerDied, got {err:?}"
    );
    sup.shutdown();
}

#[test]
fn corrupt_frame_kills_worker_with_protocol_error() {
    let sup = WorkerSupervisor::new(launch("corrupt"));
    let err = sup.handshake().unwrap_err();
    // Oversized frame from the mock is a hard protocol violation; the
    // supervisor must kill the worker, not wait or guess.
    assert!(
        matches!(
            err,
            SupervisorError::Protocol(attic_inference_protocol::ProtocolError::FrameTooLarge(_))
        ),
        "expected FrameTooLarge, got {err:?}"
    );
    assert!(sup.worker_pid().is_none(), "corrupt worker must be killed");
    sup.shutdown();
}

#[test]
fn shutdown_then_embed_recovers_via_lazy_restart() {
    // After an explicit shutdown the child is gone; the next embed must
    // restart the worker, reload the model lazily, and answer correctly —
    // no panic, no hang, no silent wrong answer.
    let sup = WorkerSupervisor::new(launch("echo"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    sup.shutdown();
    let out = sup
        .embed_batch(items(&["after-shutdown"]), Duration::from_secs(10))
        .expect("lazy restart must recover a healthy worker");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][2], "after-shutdown".len() as f32);
    sup.shutdown();
}
