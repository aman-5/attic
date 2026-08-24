//! Diagnostic types produced during discovery walks.
//!
//! Diagnostics are non-fatal observations about the walk: symlink escapes,
//! IO errors, unstable captures, etc.  They are collected alongside the
//! eligible entry list so callers can log, surface, or act on them without
//! the walk itself failing.

use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Classification of a diagnostic event.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DiagnosticKind {
    /// A symlink resolved to a path outside the allowed root boundary.
    SymlinkEscape,
    /// A symlink chain could not be resolved (possible cycle or broken link).
    SymlinkCycle,
    /// A file changed (size or mtime) between the start and end of a walk,
    /// indicating a potentially unstable capture window.
    UnstableCapture,
    /// A file system IO error was encountered traversing an entry.
    IoError,
    /// A Git submodule was detected; sub-module roots are excluded from the
    /// parent walk (OQ-004 resolution: each submodule is treated as a
    /// separate repository).
    SubmoduleDetected,
    /// A path that was listed in `scan_exempt_paths` was found on disk but
    /// overlapped with a security-forbidden prefix — the exemption was
    /// rejected.
    ExemptionRejected,
}

/// A single non-fatal observation produced during a discovery walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What kind of event occurred.
    pub kind: DiagnosticKind,
    /// The filesystem path the event relates to (may be empty for global events).
    pub path: PathBuf,
    /// Human-readable description suitable for logging.
    pub message: String,
}

impl Diagnostic {
    /// Convenience constructor.
    pub fn new(kind: DiagnosticKind, path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self {
            kind,
            path: path.into(),
            message: message.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn diagnostic_new_constructor() {
        let d = Diagnostic::new(
            DiagnosticKind::SymlinkEscape,
            PathBuf::from("/some/path"),
            "escaped root",
        );
        assert_eq!(d.kind, DiagnosticKind::SymlinkEscape);
        assert_eq!(d.path, PathBuf::from("/some/path"));
        assert_eq!(d.message, "escaped root");
    }

    #[test]
    fn diagnostic_kinds_are_distinct() {
        assert_ne!(DiagnosticKind::SymlinkEscape, DiagnosticKind::SymlinkCycle);
        assert_ne!(DiagnosticKind::IoError, DiagnosticKind::UnstableCapture);
    }
}
