//! Attic — file discovery and security boundary skeleton.
//!
//! Implements: repository discovery, `.gitignore` / `.git/info/exclude`
//! semantics, configurable discovery policy, canonical path validation,
//! symlink protection, and file classification.
//!
//! # Bootstrap state
//! Skeleton only.  Git-aware walker and security checks added in Phase 1B.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

/// Placeholder module.  Real discovery logic is added in Phase 1B.
pub mod placeholder {
    /// Bootstrap placeholder — removed when Phase 1B begins.
    pub fn discovery_crate_present() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder::discovery_crate_present;

    #[test]
    fn crate_is_present() {
        assert!(discovery_crate_present());
    }
}
