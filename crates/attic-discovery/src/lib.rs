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
//! - any non-fatal [`Diagnostic`] events.

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
// High-level API
// ---------------------------------------------------------------------------

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
}

/// Run the full discovery pipeline over `root`.
///
/// # Steps
///
/// 1. Canonicalise `root` (rejects path-traversal attempts).
/// 2. Detect the Git repository root (if any).
/// 3. Walk the tree with the `ignore` crate using `policy`.
/// 4. Apply security exclusions, default exclusions, and classification.
/// 5. Build the BLAKE3 manifest.
/// 6. Return [`DiscoveryOutput`].
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

    // 5. Build the BLAKE3 manifest.
    let manifest = manifest::build_manifest(&walk_result.entries, &canonical_root);

    // Collect manifest read errors as diagnostics too.
    all_diagnostics.extend(manifest.read_errors.clone());

    Ok(DiscoveryOutput {
        entries: walk_result.entries,
        manifest,
        git_meta,
        diagnostics: all_diagnostics,
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
}
