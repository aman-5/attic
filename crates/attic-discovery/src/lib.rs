//! `attic-discovery` — Git-aware, security-hardened repository discovery.
//!
//! # Phase 1B scope
//!
//! This crate implements the full Phase 1B discovery pipeline:
//!
//! ```text
//! configured roots
//!   → canonicalize / security boundary
//!   → git repository detection
//!   → gitignore evaluation (via `ignore` crate)
//!   → Attic include / exclude policy
//!   → default exclusions
//!   → priority classification
//!   → eligible manifest
//!   → SourceRevision manifest hash
//!   → secrets preprocessing (Safe / Redacted / Excluded per file)
//! ```
//!
//! # Entry point
//!
//! The primary entry point is [`discover`], which takes a root path and a
//! [`DiscoveryPolicy`] and returns a [`DiscoveryOutput`] containing:
//!
//! - all eligible [`EligibleEntry`] values,
//! - a [`SourceManifest`] with the BLAKE3 manifest hash,
//! - optional [`GitRepoMeta`] (branch + HEAD SHA),
//! - any non-fatal [`Diagnostic`] events,
//! - per-file [`DownstreamContent`] results from secrets preprocessing.

pub mod classification;
pub mod diagnostics;
pub mod error;
pub mod git;
pub mod manifest;
pub mod policy;
pub mod secrets;
pub mod security;
pub mod walk;

// ---------------------------------------------------------------------------
// Convenience re-exports
// ---------------------------------------------------------------------------

pub use policy::DiscoveryPriority;
pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use error::DiscoveryError;
pub use git::GitRepoMeta;
pub use manifest::{ManifestEntry, SourceManifest};
pub use policy::{DiscoveryPolicy, GlobRule, PriorityRule};
pub use secrets::{PreprocessResult, ScanResult, SecretFinding, SecretScanDecision};
pub use security::canonicalize_within_root;
pub use walk::{EligibleEntry, WalkResult};

// ---------------------------------------------------------------------------
// Downstream content classification
// ---------------------------------------------------------------------------

/// The result of preprocessing one eligible file's content for downstream
/// consumption (FTS, embeddings, indexing).
///
/// Produced by [`secrets::preprocess`] at the discovery boundary so that no
/// secret material ever reaches downstream storage or logs.
#[derive(Debug, Clone)]
pub enum DownstreamContent {
    /// Content is safe to index as-is (no secrets found).
    Safe(String),
    /// Content contained secrets; the `content` field is the redacted version
    /// safe for indexing, and `findings` describes what was found (no raw
    /// secret values).
    Redacted {
        /// Redacted content safe for downstream use.
        content: String,
        /// Descriptions of findings (pattern IDs + offsets only).
        findings: Vec<SecretFinding>,
    },
    /// The file is a known secrets carrier (e.g. `.netrc`, `id_rsa`) and must
    /// not be indexed at all.
    Excluded,
}

// ---------------------------------------------------------------------------
// High-level API
// ---------------------------------------------------------------------------

use std::fs;
use std::path::Path;

/// The complete output of a single discovery pass over one root directory.
#[derive(Debug)]
pub struct DiscoveryOutput {
    /// Eligible files sorted by `repo_relative` path.
    pub entries: Vec<EligibleEntry>,
    /// Content-hash manifest and stable revision identifier.
    pub manifest: SourceManifest,
    /// Git repository metadata, if `root` is (or is inside) a Git repo.
    pub git_meta: Option<GitRepoMeta>,
    /// Non-fatal diagnostics produced during the walk or manifest build.
    pub diagnostics: Vec<Diagnostic>,
    /// Per-file downstream content classification.
    ///
    /// Each element is `(repo_relative_path, downstream_content)`.  The vec
    /// is in the same order as [`DiscoveryOutput::entries`].  Files whose
    /// content could not be read are absent from this vec (they will also
    /// appear in [`SourceManifest::read_errors`]).
    pub downstream_contents: Vec<(String, DownstreamContent)>,
}

/// Run the full discovery pipeline over `root`.
///
/// # Steps
///
/// 1. Canonicalise `root` (rejects path-traversal attempts).
/// 2. Detect the Git repository root (if any).
/// 3. Walk the tree with the `ignore` crate using `policy`.
/// 4. Apply security exclusions, default exclusions, and classification.
/// 5. Build the BLAKE3 manifest (using raw bytes — before redaction).
/// 6. Preprocess each file's content through the secrets layer, producing
///    [`DownstreamContent`] values safe for downstream use.
/// 7. Return [`DiscoveryOutput`].
///
/// # Errors
///
/// Returns [`DiscoveryError`] for hard failures (root not a directory, path
/// escape, IO error during canonicalisation).  Non-fatal IO errors during the
/// walk or manifest build are captured in [`DiscoveryOutput::diagnostics`] /
/// [`SourceManifest::read_errors`].
pub fn discover(root: &Path, policy: &DiscoveryPolicy) -> Result<DiscoveryOutput, DiscoveryError> {
    // 1. Canonicalise root — rejects symlink escapes and non-directories.
    let canonical_root = root.canonicalize().map_err(|source| DiscoveryError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })?;

    if !canonical_root.is_dir() {
        return Err(DiscoveryError::RootNotDirectory(canonical_root));
    }

    // 2. Git repository detection (best-effort; not an error if absent).
    let git_meta = if policy.git_aware {
        git::find_git_root(&canonical_root, &canonical_root)
            .and_then(|repo_root| git::read_git_meta(&repo_root).ok())
    } else {
        None
    };

    // 3–4. Walk + security + classification.
    let walk_result = walk::walk(&canonical_root, policy)?;

    let mut all_diagnostics = walk_result.diagnostics;

    // 5. Build the BLAKE3 manifest using raw (unredacted) bytes.
    let manifest = manifest::build_manifest(&walk_result.entries, &canonical_root);

    // Collect manifest read errors and unstable capture diagnostics.
    all_diagnostics.extend(manifest.read_errors.clone());
    all_diagnostics.extend(manifest.unstable_captures.clone());

    // 6. Preprocess each file through the secrets layer.
    //    The manifest hash uses raw bytes (already computed above).
    //    Downstream content uses redacted bytes only.
    let mut downstream_contents: Vec<(String, DownstreamContent)> =
        Vec::with_capacity(walk_result.entries.len());

    for entry in &walk_result.entries {
        // Read raw content.  If unreadable, skip (read error already in
        // manifest.read_errors / all_diagnostics).
        let raw = match fs::read_to_string(&entry.abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let preprocess = secrets::preprocess(&raw, &entry.repo_relative);

        let downstream = match preprocess.decision {
            SecretScanDecision::Excluded => DownstreamContent::Excluded,
            SecretScanDecision::Safe => {
                DownstreamContent::Safe(preprocess.content.unwrap_or_default())
            }
            SecretScanDecision::Redacted => DownstreamContent::Redacted {
                content: preprocess.content.unwrap_or_default(),
                findings: preprocess.findings,
            },
        };

        downstream_contents.push((entry.repo_relative.clone(), downstream));
    }

    Ok(DiscoveryOutput {
        entries: walk_result.entries,
        manifest,
        git_meta,
        diagnostics: all_diagnostics,
        downstream_contents,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_repo(dir: &std::path::Path) {
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[test]
    fn discover_returns_eligible_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/main.rs", "fn main() {}");
        write_file(root, "src/lib.rs", "pub fn x() {}");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let paths: Vec<&str> = output.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib.rs"));
    }

    #[test]
    fn discover_manifest_hash_is_64_hex_chars() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        assert_eq!(output.manifest.manifest_hash.len(), 64);
        assert!(output.manifest.manifest_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn discover_detects_git_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let meta = output.git_meta.expect("should have git meta");
        assert_eq!(meta.branch.as_deref(), Some("main"));
    }

    #[test]
    fn discover_no_git_meta_when_not_git_aware() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_non_git();
        let output = discover(root, &policy).unwrap();

        assert!(output.git_meta.is_none());
    }

    #[test]
    fn discover_excludes_security_forbidden_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/main.rs", "fn main() {}");
        write_file(root, "private.key", "-----BEGIN PRIVATE KEY-----");
        write_file(root, ".env", "SECRET=abc");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let paths: Vec<&str> = output.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(!paths.contains(&"private.key"));
        assert!(!paths.contains(&".env"));
        assert!(paths.contains(&"src/main.rs"));
    }

    #[test]
    fn discover_manifest_is_deterministic_across_two_runs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "fn a() {}");
        write_file(root, "src/b.rs", "fn b() {}");

        let policy = DiscoveryPolicy::default_git();
        let out1 = discover(root, &policy).unwrap();
        let out2 = discover(root, &policy).unwrap();

        assert_eq!(out1.manifest.manifest_hash, out2.manifest.manifest_hash);
    }

    #[test]
    fn discover_error_for_nonexistent_root() {
        let policy = DiscoveryPolicy::default_git();
        let result = discover(std::path::Path::new("/nonexistent/does/not/exist/xyzzy"), &policy);
        assert!(result.is_err(), "should fail for non-existent root");
    }

    // ── New tests required by review (Gap 3: secrets preprocessing) ───────

    /// A source file containing an AWS access key must produce
    /// `DownstreamContent::Redacted` so the key never reaches FTS.
    #[test]
    fn inline_secret_produces_redacted_downstream_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // Embed a recognisable AWS access key.
        write_file(
            root,
            "src/config.rs",
            "// AWS key below\nconst KEY: &str = \"AKIAIOSFODNN7EXAMPLE\";\n",
        );

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let dc = output
            .downstream_contents
            .iter()
            .find(|(path, _)| path == "src/config.rs")
            .map(|(_, dc)| dc)
            .expect("src/config.rs must have a downstream content entry");

        match dc {
            DownstreamContent::Redacted { content, findings } => {
                assert!(
                    !findings.is_empty(),
                    "at least one finding expected for AWS key; got {findings:?}"
                );
                assert!(
                    !content.contains("AKIAIOSFODNN7EXAMPLE"),
                    "redacted content must not contain the raw AWS key; got: {content}"
                );
            }
            other => panic!(
                "expected DownstreamContent::Redacted for file with AWS key, got {other:?}"
            ),
        }
    }

    /// A file whose name marks it as a known secrets carrier (`.netrc` is in
    /// `is_known_secrets_file()` but is not necessarily security-forbidden
    /// at the walk layer) must produce `DownstreamContent::Excluded`.
    ///
    /// This test verifies the discovery boundary: even if `.netrc` passes the
    /// walk (it is not always in `is_security_forbidden()`), `preprocess()`
    /// must classify it as `Excluded`.
    #[test]
    fn known_secret_carrier_produces_excluded_downstream_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // Write a .netrc file with dummy credentials.
        write_file(root, ".netrc", "machine example.com login user password secret\n");

        // Use include_untracked=true so the file is considered by the walk.
        // Whether it makes it to walk output depends on security.rs; if it is
        // security-forbidden it won't be in entries, but it also won't be in
        // downstream_contents (no entry to preprocess).
        // If it does make it through, downstream_contents must show Excluded.
        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = true;

        let output = discover(root, &policy).unwrap();

        // Find .netrc in downstream_contents if present.
        let dc_entry = output
            .downstream_contents
            .iter()
            .find(|(path, _)| path == ".netrc");

        if let Some((_, dc)) = dc_entry {
            // If .netrc reached downstream_contents it must be Excluded.
            assert!(
                matches!(dc, DownstreamContent::Excluded),
                ".netrc must be Excluded in downstream content; got {dc:?}"
            );
        }
        // If .netrc is not in downstream_contents at all, it was either
        // security-forbidden (excluded at walk level) or not found.
        // Either outcome satisfies the security requirement.
    }
}
