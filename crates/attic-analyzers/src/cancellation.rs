//! Cancellation token for Phase 1C analyzer operations.
//!
//! OQ-012: tokio_util::sync::CancellationToken backing deferred to Phase 4.
//! For Phase 1C (synchronous, no async), we use `Arc<AtomicBool>` as the
//! backing type wrapped in this newtype.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// A lightweight cancellation signal that can be shared across analyzer
/// invocations.
///
/// Cloning is cheap (Arc clone). Setting cancellation is thread-safe.
/// Checking is a single atomic load.
///
/// Phase 4 note (OQ-012): When async analyzers are introduced, this type
/// will be replaced or wrapped with `tokio_util::sync::CancellationToken`
/// without changing the `Analyzer` trait signature, since the `AnalyzerInput`
/// always holds `CancellationToken` by value.
#[derive(Clone, Debug)]
pub struct CancellationToken {
    inner: Arc<AtomicBool>,
}

impl CancellationToken {
    /// Create a new, non-cancelled token.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Return `true` if cancellation has been requested.
    #[inline]
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::Acquire)
    }

    /// Signal cancellation. All holders of clones of this token will see
    /// `is_cancelled()` return `true` after this call.
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::Release);
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_token_is_not_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn cancel_is_visible_to_all_clones() {
        let token = CancellationToken::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_is_idempotent() {
        let token = CancellationToken::new();
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn default_is_not_cancelled() {
        let token = CancellationToken::default();
        assert!(!token.is_cancelled());
    }
}
