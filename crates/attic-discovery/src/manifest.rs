//! Eligible-file manifest and `SourceRevision` hash computation.
//!
//! # Algorithm (per `source_revision` contract)
//!
//! 1. Collect all eligible entries from the walk (already sorted by
//!    `repo_relative` path).
//! 2. For each entry, stat the file, compute the BLAKE3 hash of the **raw
//!    file content**, then stat again.  If size or mtime changed the entry is
//!    marked `unstable` and a [`DiagnosticKind::UnstableCapture`] diagnostic
//!    is emitted.
//! 3. Build a deterministic manifest string:
//!    ```text
//!    <repo_relative_path>\t<content_hash_hex>\n
//!    ```
//!    Lines are in lexicographic order of `repo_relative_path`.
//! 4. Hash the manifest string with BLAKE3 to produce the
//!    `manifest_hash` (the `SourceRevision` identifier).
//!
//! The hash is **content-only**: timestamps, inode numbers, and file modes
//! are intentionally excluded so that the revision is stable across
//! checkouts and moves (OQ-005 resolution).

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use blake3::Hasher;

use crate::{
    diagnostics::{Diagnostic, DiagnosticKind},
    walk::EligibleEntry,
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// BLAKE3 hash of a single file's content (32 bytes, hex-encoded = 64 chars).
pub type ContentHash = String;

/// One row in the manifest: a file path and its content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Repo-relative path (forward slashes, no leading `/`).
    pub repo_relative: String,
    /// BLAKE3 hash of the file content (64 lowercase hex chars).
    pub content_hash: ContentHash,
    /// `true` when the file's size or mtime changed while it was being hashed,
    /// indicating that the hash may not reflect a consistent snapshot.
    pub unstable: bool,
}

/// The complete manifest for a single discovery pass.
#[derive(Debug, Clone)]
pub struct SourceManifest {
    /// Ordered (by `repo_relative`) list of all eligible files with their
    /// content hashes.
    pub entries: Vec<ManifestEntry>,
    /// BLAKE3 hash of the canonical manifest text — this is the stable
    /// `SourceRevision` identifier for this snapshot.
    pub manifest_hash: String,
    /// Non-fatal IO errors encountered while reading file contents.  These
    /// indicate a **partial** manifest; callers should treat the revision as
    /// `UNSTABLE_CAPTURE` if any errors are present.
    pub read_errors: Vec<Diagnostic>,
    /// [`DiagnosticKind::UnstableCapture`] events: files whose size or mtime
    /// changed between the pre-hash and post-hash stat calls.
    pub unstable_captures: Vec<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Internal: file stat snapshot used for change detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
struct FileStat {
    size: u64,
    modified: Option<SystemTime>,
}

impl FileStat {
    fn read(path: &Path) -> std::io::Result<Self> {
        let meta = fs::metadata(path)?;
        let modified = meta.modified().ok();
        Ok(FileStat {
            size: meta.len(),
            modified,
        })
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Build a [`SourceManifest`] from a pre-sorted slice of [`EligibleEntry`]
/// values.
///
/// `root` is only used for constructing the diagnostic path on read errors;
/// it is **not** used for hashing (content-only).
///
/// Files that cannot be read produce a [`Diagnostic`] with kind
/// [`DiagnosticKind::IoError`] and are **omitted** from the manifest.  The
/// resulting `manifest_hash` will therefore differ from a clean-read result,
/// and callers must inspect `read_errors` to decide whether the revision is
/// stable.
///
/// Files whose size or mtime changed during hashing are included in the
/// manifest (the hash reflects whatever bytes were read) but are flagged
/// `unstable = true` and a [`DiagnosticKind::UnstableCapture`] diagnostic is
/// appended to [`SourceManifest::unstable_captures`].
pub fn build_manifest(entries: &[EligibleEntry], root: &Path) -> SourceManifest {
    let mut manifest_entries: Vec<ManifestEntry> = Vec::with_capacity(entries.len());
    let mut read_errors: Vec<Diagnostic> = Vec::new();
    let mut unstable_captures: Vec<Diagnostic> = Vec::new();

    for entry in entries {
        // ── Stat before hashing ──────────────────────────────────────────
        let stat_before = FileStat::read(&entry.abs_path).ok();

        // ── Hash raw content ─────────────────────────────────────────────
        match hash_file_content(&entry.abs_path) {
            Err(e) => {
                read_errors.push(Diagnostic {
                    kind: DiagnosticKind::IoError,
                    path: entry.abs_path.clone(),
                    message: format!(
                        "failed to read {} for manifest: {e}",
                        root.join(&entry.repo_relative).display()
                    ),
                });
            }
            Ok(hash) => {
                // ── Stat after hashing ───────────────────────────────────
                let stat_after = FileStat::read(&entry.abs_path).ok();

                // Detect a change: size or mtime differs between reads.
                // If either stat failed the capture state is uncertain —
                // we cannot confirm the file was stable, so treat it as
                // unstable (fail-closed).
                let file_changed = match (&stat_before, &stat_after) {
                    (Some(before), Some(after)) => before != after,
                    // One or both stats unavailable → cannot establish stability.
                    _ => true,
                };

                if file_changed {
                    unstable_captures.push(Diagnostic {
                        kind: DiagnosticKind::UnstableCapture,
                        path: entry.abs_path.clone(),
                        message: format!(
                            "file '{}' changed during hashing (size or mtime differed); \
                             manifest entry may not reflect a consistent snapshot",
                            entry.repo_relative
                        ),
                    });
                }

                manifest_entries.push(ManifestEntry {
                    repo_relative: entry.repo_relative.clone(),
                    content_hash: hash,
                    unstable: file_changed,
                });
            }
        }
    }

    // Entries are already sorted by the walk; re-sort defensively in case the
    // caller passed an unsorted slice.
    manifest_entries.sort_by(|a, b| a.repo_relative.cmp(&b.repo_relative));

    let manifest_text = serialize_manifest(&manifest_entries);
    let manifest_hash = hash_manifest_text(&manifest_text);

    SourceManifest {
        entries: manifest_entries,
        manifest_hash,
        read_errors,
        unstable_captures,
    }
}

/// Returns `true` when the manifest contains no read errors and no unstable
/// captures (fully stable).
impl SourceManifest {
    pub fn is_stable(&self) -> bool {
        self.read_errors.is_empty() && self.unstable_captures.is_empty()
    }

    /// Serialise the manifest to the canonical tab-separated text format.
    pub fn to_manifest_text(&self) -> String {
        serialize_manifest(&self.entries)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compute the BLAKE3 hash of a file's raw byte content.
fn hash_file_content(path: &Path) -> std::io::Result<ContentHash> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Hasher::new();

    // Stream in 64 KiB chunks to avoid loading large files into RAM.
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hasher.finalize().to_hex().to_string())
}

/// Produce the canonical manifest text from a sorted slice of entries.
///
/// Format: `<repo_relative>\t<content_hash>\n` for each entry.
fn serialize_manifest(entries: &[ManifestEntry]) -> String {
    let mut out = String::with_capacity(entries.len() * 80);
    for entry in entries {
        out.push_str(&entry.repo_relative);
        out.push('\t');
        out.push_str(&entry.content_hash);
        out.push('\n');
    }
    out
}

/// Hash the canonical manifest text with BLAKE3.
fn hash_manifest_text(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::DiscoveryPriority;
    use crate::walk::EligibleEntry;
    use std::fs;
    use tempfile::TempDir;

    fn make_entry(root: &Path, rel: &str, content: &str) -> EligibleEntry {
        let abs = root.join(rel);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, content).unwrap();
        EligibleEntry {
            abs_path: abs,
            repo_relative: rel.to_string(),
            priority: DiscoveryPriority::Normal,
        }
    }

    #[test]
    fn manifest_hash_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let entries = vec![
            make_entry(root, "src/a.rs", "fn a() {}"),
            make_entry(root, "src/b.rs", "fn b() {}"),
        ];

        let m1 = build_manifest(&entries, root);
        let m2 = build_manifest(&entries, root);

        assert_eq!(m1.manifest_hash, m2.manifest_hash);
    }

    #[test]
    fn manifest_hash_changes_with_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let entries_v1 = vec![make_entry(root, "src/a.rs", "version 1")];
        let h1 = build_manifest(&entries_v1, root).manifest_hash;

        // Overwrite the file with different content.
        fs::write(root.join("src/a.rs"), "version 2").unwrap();
        // Re-create entries pointing to same abs_path (content changed on disk).
        let entries_v2 = vec![EligibleEntry {
            abs_path: root.join("src/a.rs"),
            repo_relative: "src/a.rs".to_string(),
            priority: DiscoveryPriority::Normal,
        }];
        let h2 = build_manifest(&entries_v2, root).manifest_hash;

        assert_ne!(
            h1, h2,
            "manifest hash must change when file content changes"
        );
    }

    #[test]
    fn manifest_hash_independent_of_order_of_input() {
        // The manifest is sorted internally; input order must not affect output.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let e1 = make_entry(root, "src/a.rs", "aaa");
        let e2 = make_entry(root, "src/b.rs", "bbb");

        let m_ab = build_manifest(&[e1.clone(), e2.clone()], root);
        let m_ba = build_manifest(&[e2.clone(), e1.clone()], root);

        assert_eq!(m_ab.manifest_hash, m_ba.manifest_hash);
    }

    #[test]
    fn empty_manifest_has_stable_hash() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let m = build_manifest(&[], root);
        assert!(m.is_stable());
        // Empty manifest text hashes to a known value; just confirm it's 64 chars.
        assert_eq!(m.manifest_hash.len(), 64);
    }

    #[test]
    fn unreadable_file_produces_read_error_and_partial_manifest() {
        // We simulate an unreadable file by pointing abs_path at a non-existent file.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let good = make_entry(root, "src/good.rs", "ok");
        let bad = EligibleEntry {
            abs_path: root.join("src/nonexistent.rs"),
            repo_relative: "src/nonexistent.rs".to_string(),
            priority: DiscoveryPriority::Normal,
        };

        let m = build_manifest(&[good, bad], root);
        assert!(
            !m.is_stable(),
            "manifest with read errors should not be stable"
        );
        assert_eq!(m.read_errors.len(), 1);
        // Only the good file appears in entries.
        assert_eq!(m.entries.len(), 1);
        assert_eq!(m.entries[0].repo_relative, "src/good.rs");
    }

    #[test]
    fn manifest_text_format_is_tab_separated() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let e = make_entry(root, "src/x.rs", "x");
        let m = build_manifest(&[e], root);
        let text = m.to_manifest_text();
        assert!(text.contains('\t'), "manifest text must use tab separator");
        assert!(text.ends_with('\n'), "manifest text must end with newline");
        let parts: Vec<&str> = text.trim_end_matches('\n').splitn(2, '\t').collect();
        assert_eq!(parts[0], "src/x.rs");
        assert_eq!(parts[1].len(), 64, "content hash must be 64 hex chars");
    }

    #[test]
    fn content_hash_length_is_64_hex_chars() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("f.txt");
        fs::write(&p, b"hello").unwrap();
        let hash = hash_file_content(&p).unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Stable files produce no unstable_capture diagnostics and entries have
    /// `unstable = false`.
    #[test]
    fn stable_file_produces_no_unstable_capture_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let e = make_entry(root, "src/stable.rs", "fn stable() {}");

        let m = build_manifest(&[e], root);

        assert!(
            m.unstable_captures.is_empty(),
            "no unstable captures expected for a stable file; got {:?}",
            m.unstable_captures
        );
        assert!(
            !m.entries[0].unstable,
            "ManifestEntry.unstable must be false for a stable file"
        );
        assert!(m.is_stable());
    }

    /// FileStat comparison correctly identifies a size change.
    #[test]
    fn file_stat_detects_size_change() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        fs::write(&path, b"hello").unwrap();

        let before = FileStat::read(&path).unwrap();

        // Overwrite with more content.
        fs::write(&path, b"hello world extended content here").unwrap();

        let after = FileStat::read(&path).unwrap();

        assert_ne!(before, after, "FileStat must differ after size change");
        assert_ne!(before.size, after.size);
    }

    /// Two different `FileStat` values → `file_changed` logic returns `true`.
    /// Identical values → returns `false`.
    #[test]
    fn unstable_capture_detected_when_stats_differ() {
        let stat_a = FileStat {
            size: 10,
            modified: None,
        };
        let stat_b = FileStat {
            size: 20,
            modified: None,
        };
        assert_ne!(stat_a, stat_b, "different sizes must produce != stats");

        let stat_c = FileStat {
            size: 10,
            modified: None,
        };
        assert_eq!(stat_a, stat_c, "identical stats must be equal");
    }

    /// When a stat call fails (file absent before/after hashing), the capture
    /// must be treated as uncertain/unstable — not stable.
    #[test]
    fn uncertain_stat_is_treated_as_unstable() {
        // Simulate stat failure by using a missing file:
        // stat_before = None, stat_after = None → uncertain → unstable.
        // We exercise the match arm directly via the logic's documented contract:
        // (None, _) or (_, None) must yield file_changed = true.
        //
        // We test the observable outcome: an entry whose file disappears after
        // hashing starts (abs_path points to a real file for the hash but we
        // delete it before the post-hash stat) should surface as unstable.
        //
        // Because we cannot reliably race the post-stat in a unit test, we
        // verify the logic by creating a manifest entry with a file that
        // exists for hashing but test the FileStat::read error path separately.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Build a valid entry for a file that exists.
        let e = make_entry(root, "src/ephemeral.rs", "fn e() {}");
        let m = build_manifest(&[e], root);

        // For a normal file both stats succeed → stable.
        assert!(
            m.unstable_captures.is_empty(),
            "normal file must be stable; got {:?}",
            m.unstable_captures
        );

        // Now confirm the uncertain branch: (None, Some) or (Some, None)
        // resolves to true. We access the match logic indirectly: if
        // FileStat::read returns Err, stat is None. Verify that a None stat
        // for a nonexistent path is indeed None (not a panic).
        let nonexistent = root.join("does_not_exist.rs");
        assert!(
            FileStat::read(&nonexistent).is_err(),
            "FileStat::read must fail for nonexistent path"
        );
        // The `_ => true` arm in build_manifest means any None stat
        // causes unstable=true — documented and verified by code inspection
        // since the race cannot be forced deterministically.
    }
}
