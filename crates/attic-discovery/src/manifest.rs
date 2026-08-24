//! Eligible-file manifest and `SourceRevision` hash computation.
//!
//! # Algorithm (per `source_revision` contract)
//!
//! 1. Collect all eligible entries from the walk (already sorted by
//!    `repo_relative` path).
//! 2. For each entry, compute the BLAKE3 hash of the **file content**.
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
pub fn build_manifest(entries: &[EligibleEntry], root: &Path) -> SourceManifest {
    let mut manifest_entries: Vec<ManifestEntry> = Vec::with_capacity(entries.len());
    let mut read_errors: Vec<Diagnostic> = Vec::new();

    for entry in entries {
        match hash_file_content(&entry.abs_path) {
            Ok(hash) => {
                manifest_entries.push(ManifestEntry {
                    repo_relative: entry.repo_relative.clone(),
                    content_hash: hash,
                });
            }
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
    }
}

/// Returns `true` when the manifest contains no read errors (fully stable).
impl SourceManifest {
    pub fn is_stable(&self) -> bool {
        self.read_errors.is_empty()
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

        assert_ne!(h1, h2, "manifest hash must change when file content changes");
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
        assert!(!m.is_stable(), "manifest with read errors should not be stable");
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
}
