//! Event normalization + early ignore/security filtering.
//!
//! Native watcher events are **hints**.  Normalization maps backend-specific
//! `notify` events into repo-relative [`NormalizedEvent`] values; the early
//! filter drops everything Attic must never touch (`.git` internals, build
//! output, dependency trees) BEFORE any queueing work happens.

use std::path::Path;

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
    if !s.is_ascii() && s.contains('\0') {
        return None;
    }
    Some(s.replace('\\', "/"))
}

/// Early ignore/security filter applied before any queueing.
///
/// Mirrors Phase 1B's hard security boundary plus the default exclusions that
/// can never be interesting to Attic.  Returns `true` when the event must be
/// DROPPED.
pub fn is_early_filtered(rel_path: &str) -> bool {
    let p = rel_path.trim_start_matches("./");
    if p.is_empty() {
        return true;
    }
    // Git internals are forbidden territory.
    if p == ".git" || p.starts_with(".git/") {
        return true;
    }
    // Never follow parent escapes.
    if p.contains("..") || p.starts_with('/') || p.contains(':') {
        return true;
    }
    // Default exclusions: build output and vendored dependencies generate
    // enormous event storms that would waste the bounded queues.
    const NOISE_PREFIXES: [&str; 8] = [
        "target/",
        "node_modules/",
        "vendor/",
        "dist/",
        "build/",
        "out/",
        ".idea/",
        "__pycache__/",
    ];
    for prefix in NOISE_PREFIXES {
        if p.starts_with(prefix) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_and_noise_are_filtered_early() {
        assert!(is_early_filtered(".git/HEAD"));
        assert!(is_early_filtered(".git"));
        assert!(is_early_filtered("target/debug/foo.rs"));
        assert!(is_early_filtered("node_modules/pkg/index.js"));
        assert!(is_early_filtered("../escape.rs"));
        assert!(is_early_filtered(""));
        assert!(!is_early_filtered("src/lib.rs"));
        assert!(!is_early_filtered(".gitignore"));
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
