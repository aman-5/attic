//! Mock inference worker binary (test fixture for supervisor fault
//! injection). Behavior selected by ATTIC_MOCK_WORKER=echo|hang|corrupt|crash.
//! The production worker entry point is `attic inference-worker` (the real
//! neural engine needs attic-semantic's providers, which cannot depend on
//! this crate without a package cycle).

fn main() {
    std::process::exit(attic_inference_protocol::engine::run_mock_worker_stdio());
}
