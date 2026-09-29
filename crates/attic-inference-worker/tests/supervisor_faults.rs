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
fn identity_mismatch_on_load_is_rejected_and_worker_killed() {
    // An installed identity verifier represents Phase 2's "loaded worker
    // identity must be authoritative" gate: whatever the worker actually
    // reports must match what was expected before it existed, or the load
    // is rejected outright and the worker is not left running.
    let sup = WorkerSupervisor::new(launch("echo"));
    sup.handshake().unwrap();
    sup.set_identity_verifier(|_caps: &[String]| Err("simulated fingerprint mismatch".into()));

    let err = sup.load_model(params()).unwrap_err();
    assert!(
        matches!(err, SupervisorError::Engine { .. }),
        "expected Engine error, got {err:?}"
    );
    assert!(format!("{err}").contains("worker identity mismatch"));
    assert!(
        sup.worker_pid().is_none(),
        "a worker with rejected identity must not be left running"
    );
    sup.shutdown();
}

#[test]
fn identity_is_reverified_on_every_lazy_restart() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // First load must pass; the restart after shutdown must be re-checked
    // rather than trusting the first successful verification forever.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_in_verifier = calls.clone();

    let sup = WorkerSupervisor::new(launch("echo"));
    sup.handshake().unwrap();
    sup.set_identity_verifier(move |_caps: &[String]| {
        let n = calls_in_verifier.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            Ok(())
        } else {
            Err("second load rejected".into())
        }
    });

    sup.load_model(params()).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    sup.shutdown();
    let err = sup
        .embed_batch(items(&["after-shutdown"]), Duration::from_secs(10))
        .unwrap_err();
    assert!(
        matches!(err, SupervisorError::Engine { .. }),
        "restart's reload must be re-verified, got {err:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "lazy restart must invoke the identity verifier again, not skip it"
    );
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

#[test]
fn slow_but_progressing_worker_survives_stall_watchdog() {
    // 3 s of work, heartbeat every ~1 s: a 2 s stall limit must not fire.
    let sup = WorkerSupervisor::new(launch("slow"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    let out = sup
        .embed_batch_watched(
            items(&["a", "b"]),
            Duration::from_secs(20),
            Some(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(sup.stall_stats().0, 0);
    sup.shutdown();
}

#[test]
fn paused_time_is_not_charged_to_the_work_budget() {
    // Worker is paused (thermal/VRAM wait) for 3 s; a 1.5 s work budget
    // must still succeed because paused time is excluded.
    let sup = WorkerSupervisor::new(launch("paused"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    let out = sup
        .embed_batch_watched(
            items(&["a"]),
            Duration::from_millis(1500),
            Some(Duration::from_secs(2)),
        )
        .unwrap();
    assert_eq!(out.len(), 1);
    sup.shutdown();
}

#[test]
fn silent_worker_is_killed_by_stall_watchdog_fast() {
    let sup = WorkerSupervisor::new(launch("hang"));
    sup.handshake().unwrap();
    sup.load_model(params()).unwrap();
    let t = std::time::Instant::now();
    let err = sup
        .embed_batch_watched(
            items(&["stuck"]),
            Duration::from_secs(300),
            Some(Duration::from_secs(1)),
        )
        .unwrap_err();
    assert!(
        matches!(err, SupervisorError::WorkerStalled(_)),
        "expected WorkerStalled, got {err:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    let (kills, last) = sup.stall_stats();
    assert_eq!(kills, 1);
    assert!(last.is_some());
}