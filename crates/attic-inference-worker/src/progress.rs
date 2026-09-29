//! Engine liveness signals for the worker's heartbeat.
//!
//! The engine runs inside a blocking native call the worker loop cannot
//! observe, so it reports liveness here: [`tick`] after each unit of real
//! work (a forward pass), [`set_paused`] around deliberate waits (thermal
//! pause, VRAM headroom). The worker loop turns these into
//! [`crate::WorkerResponse::Progress`] frames; the supervisor kills a worker
//! that goes silent.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static TICKS: AtomicU64 = AtomicU64::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Record one unit of completed work.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Mark the engine as deliberately waiting (or no longer waiting).
pub fn set_paused(paused: bool) {
    PAUSED.store(paused, Ordering::Relaxed);
}

/// `(ticks, paused)` right now.
pub fn snapshot() -> (u64, bool) {
    (TICKS.load(Ordering::Relaxed), PAUSED.load(Ordering::Relaxed))
}

/// Clears `paused` on drop, so an early return or panic inside a wait can
/// never leave the worker reporting "paused" forever.
pub struct PauseGuard(());

impl PauseGuard {
    /// Enter a deliberate wait.
    pub fn enter() -> Self {
        set_paused(true);
        Self(())
    }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        set_paused(false);
    }
}
