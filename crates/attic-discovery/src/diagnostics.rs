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

/// Cheap structured counters accumulated during a discovery walk, so callers
/// can answer "why did the indexed count differ from the filesystem count"
/// without reading server logs. These are incremented inline at the same
/// decision points that already produce [`Diagnostic`]s or filter
/// [`crate::EligibleEntry`]s — no second traversal is performed to compute
/// them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct WalkCounters {
    /// Directories the walker descended into (the root itself excluded).
    pub directories_visited: u64,
    /// Non-directory entries the walker reached, before any exclusion.
    pub files_seen: u64,
    /// Files that passed every filtering stage and became eligible entries.
    pub files_eligible: u64,
    /// Files excluded by policy (gitignore, default exclusions, priority
    /// classification, tracked-file filtering, submodule boundaries) — as
    /// opposed to an operational failure.
    pub ignored_or_pruned: u64,
    /// Submodule / nested-repository roots detected and not descended into.
    pub nested_repo_boundaries: u64,
    /// Symlinks skipped (cycle, unresolvable, or escaping the root).
    pub symlinks_skipped: u64,
    /// Files excluded by the absolute security-forbidden path policy.
    pub security_exclusions: u64,
    /// Operational failures encountered while traversing (e.g. walker IO
    /// errors) — distinct from a policy exclusion.
    pub transient_failures: u64,
    /// PR-8: bytes read while classifying SMALL files for secret scanning
    /// during discovery. Compared against `attic-indexing`'s own read of
    /// the same files during analysis, this is the measurable size of the
    /// duplicate-read the audit flagged — kept as a counter (not fixed by
    /// caching content across the discovery/indexing boundary) because an
    /// unbounded repository-wide content cache is explicitly disallowed.
    pub small_file_bytes_read: u64,
    /// Number of SMALL files whose content was fully read during discovery
    /// classification (companion to `small_file_bytes_read`).
    pub small_file_reads: u64,
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
