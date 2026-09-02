//! Lightweight cooperative cancellation shared by discovery, indexing and analyzers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A lightweight, cloneable cooperative-cancellation signal.
///
/// Clones share the same underlying flag, so cancellation requested by one
/// owner is observed by discovery, indexing, analyzers, and lifecycle tasks.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Creates a token in the non-cancelled state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
        }
    }
    /// Returns `true` after cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }
    /// Requests cancellation for this token and all of its clones.
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::Release);
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
