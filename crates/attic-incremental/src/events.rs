//! Event normalization + Phase 1B-derived filtering.
//!
//! Native watcher events are **hints**.  Normalization maps backend-specific
//! `notify` events into repo-relative [`NormalizedEvent`] values.
//!
//! Filtering has two strictly separated layers:
//!
//! 1. **Security (absolute, never policy-dependent)** — `.git` internals,
//!    parent escapes, NUL bytes, empty paths.  These mirror the mandatory
//!    Phase 1B security boundary and can never be overridden.
//! 2. **Eligibility (policy-derived)** — delegated entirely to the approved
//!    Phase 1B classifier ([`attic_discovery::classification::classify`]) so
//!    default exclusions, attic exclude rules, and attic INCLUDE rules behave
//!    identically to the walker.  A configured include rule (e.g. re-including
//!    vendored or generated content) keeps those paths visible to incremental
//!    updates.

use std::path::Path;

use attic_discovery::{DiscoveryPolicy, DiscoveryPriority, classification::classify};
use notify_debouncer_full::notify::EventKind;
use notify_debouncer_full::{DebouncedEvent, notify::event::RenameMode};

/// Normalized per-path event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsEventKind {
    /// Path appeared.
    Created,
    /// Path content probably changed (hint only).
    Modified,
    /// Path disappeared.
    Removed,
    /// Old path of an observed rename.
    RenamedFrom,
    /// New path of an observed rename.
    RenamedTo,
    /// Anything the backend could not classify; treated as a modify hint.
    Other,
}

/// One normalized, repo-relative event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEvent {
    /// Repo-relative path with forward slashes.  Directory-less: events for
    /// directories are dropped at normalization time.
    pub rel_path: String,
    /// What the watcher claims happened.
    pub kind: FsEventKind,
}

/// Policy-aware event gate derived from the approved Phase 1B
/// [`DiscoveryPolicy`] — no independent ignore policy lives here.
#[derive(Debug, Clone)]
pub struct EventFilter {
    policy: DiscoveryPolicy,
}

impl EventFilter {
    /// Build the gate from the active discovery policy.
    pub fn new(policy: DiscoveryPolicy) -> Self {
        Self { policy }
    }

    /// The underlying policy.
    pub fn policy(&self) -> &DiscoveryPolicy {
        &self.policy
    }

    /// Absolute security rejection (never policy-dependent, never overridable).
    pub fn is_security_blocked(rel_path: &str) -> bool {
        let p = rel_path.trim_start_matches("./");
        if p.is_empty() {
            return true;
        }
        // Git internals are forbidden territory (Phase 1B mandatory rule).
        if p == ".git" || p.starts_with(".git/") {
            return true;
        }
        // Never follow parent escapes / absolute / NUL-bearing paths.
        if p.contains("..") || p.starts_with('/') || p.contains(':') || p.contains('\0') {
            return true;
        }
        false
    }

    /// Phase 1B eligibility: `false` ⇒ the path would not be indexed by the
    /// approved walker either (default exclusions, attic excludes), unless an
    /// attic INCLUDE rule rescues it — in which case it stays visible here.
    pub fn is_eligible(&self, rel_path: &str) -> bool {
        classify(rel_path, &self.policy) != DiscoveryPriority::Ignored
    }
}

/// Map one debounced batch entry to normalized events (0..n paths).
pub fn normalize_debounced(event: &DebouncedEvent, root: &Path) -> Vec<NormalizedEvent> {
    let mut out = Vec::with_capacity(event.event.paths.len());
    let kinds: &[FsEventKind] = match &event.event.kind {
        EventKind::Create(_) => &[FsEventKind::Created],
        EventKind::Modify(notify_debouncer_full::notify::event::ModifyKind::Name(mode)) => {
            match mode {
                RenameMode::From => &[FsEventKind::RenamedFrom],
                RenameMode::To => &[FsEventKind::RenamedTo],
                // "Both"/"Any"/"Other" carry both endpoints in paths order.
                _ => &[FsEventKind::RenamedFrom, FsEventKind::RenamedTo],
            }
        }
        EventKind::Remove(_) => &[FsEventKind::Removed],
        EventKind::Modify(_) => &[FsEventKind::Modified],
        _ => &[FsEventKind::Other],
    };

    for (idx, path) in event.event.paths.iter().enumerate() {
        if path.is_dir() {
            continue;
        }
        let Some(rel) = to_rel_path(path, root) else {
            continue;
        };
        let kind = kinds
            .get(idx.min(kinds.len() - 1))
            .copied()
            .unwrap_or(FsEventKind::Other);
        out.push(NormalizedEvent {
            rel_path: rel,
            kind,
        });
    }
    out
}

/// Convert an absolute watched path into a normalized repo-relative path.
///
/// Returns `None` for anything outside the root (defence in depth — a watcher
/// hint must never widen the security boundary).
fn to_rel_path(path: &Path, root: &Path) -> Option<String> {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let candidate = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let rel = candidate.strip_prefix(&canonical_root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let s = rel.to_string_lossy();
    if s.contains('\0') {
        return None;
    }
    Some(s.replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(policy: DiscoveryPolicy) -> EventFilter {
        EventFilter::new(policy)
    }

    #[test]
    fn git_and_escapes_are_absolutely_blocked() {
        assert!(EventFilter::is_security_blocked(".git/HEAD"));
        assert!(EventFilter::is_security_blocked(".git"));
        assert!(EventFilter::is_security_blocked("../escape.rs"));
        assert!(EventFilter::is_security_blocked("/abs.rs"));
        assert!(EventFilter::is_security_blocked(""));
        // Security blocking is independent of any policy:
        let p = DiscoveryPolicy::default_non_git();
        assert!(EventFilter::is_security_blocked(".git/config"));
        let _ = p;
    }

    #[test]
    fn eligibility_is_policy_derived_not_hardcoded() {
        // With defaults, node_modules content is ignored...
        let f = filter(DiscoveryPolicy::default_git());
        assert!(!f.is_eligible("node_modules/pkg/index.js"));
        assert!(
            f.is_eligible("vendor/lib/x.js"),
            "vendor is NOT in Phase 1B defaults"
        );
        assert!(f.is_eligible("src/lib.rs"));
        assert!(f.is_eligible(".gitignore"));

        // ...but an explicit include rule MUST rescue even ignored paths
        // (Phase 1B parity with the walker):
        let mut p = DiscoveryPolicy::default_git();
        p.attic_include_rules
            .push(attic_discovery::GlobRule::include("node_modules/**"));
        let f2 = filter(p);
        assert!(
            f2.is_eligible("node_modules/pkg/index.js"),
            "configured includes must survive watcher filtering"
        );

        // And disabling default exclusions makes them eligible wholesale:
        let mut p3 = DiscoveryPolicy::default_git();
        p3.default_exclusions = false;
        assert!(filter(p3).is_eligible("node_modules/x/index.js"));
    }

    #[test]
    fn normalize_drops_directories_and_out_of_root_paths() {
        let root = std::env::temp_dir();
        let ev = DebouncedEvent {
            event: notify_debouncer_full::notify::Event::new(EventKind::Other).add_path(
                root.parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or(root.clone()),
            ),
            time: std::time::Instant::now(),
        };
        // Out-of-root path normalizes to nothing.
        assert!(normalize_debounced(&ev, &root).is_empty());
    }
}
