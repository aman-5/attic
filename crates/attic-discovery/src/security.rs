//! Security boundary enforcement for the discovery pipeline.
//!
//! Invariants (from discovery contract §Security Exclusions):
//! - Security exclusions are ALWAYS enforced regardless of any other rule.
//! - No include rule can override a security-forbidden path.
//! - Symlinks must not escape the configured allowed root.
//! - Path traversal (`..`) must not escape the allowed root.
//! - Scan-exempt paths cannot overlap security-forbidden prefixes.

use std::path::{Path, PathBuf};

use crate::error::DiscoveryError;

// ---------------------------------------------------------------------------
// Security-forbidden path components (contract §Security Exclusions)
// ---------------------------------------------------------------------------

/// Path components / file-name patterns that are ALWAYS security-excluded.
/// These are matched against repo-relative normalized path segments.
///
/// Rules (applied to normalized repo-relative forward-slash paths):
///   - Exact directory name at any nesting level
///   - Glob-suffix file extensions
///   - Exact file names
const FORBIDDEN_DIRS: &[&str] = &[
    ".git",        // entire .git tree (internal objects, refs, etc.)
    ".ssh",        // SSH keys
    ".gnupg",      // GPG keys
];

/// File extensions whose files are always security-excluded.
/// Matched against the lowercase file extension (without leading dot).
const FORBIDDEN_EXTENSIONS: &[&str] = &[
    "pem",  // PEM-encoded keys
    "key",  // private key files (heuristic)
    "p12",  // PKCS#12 bundles
    "jks",  // Java keystores
];

/// Exact file names (at any depth) that are always security-excluded.
const FORBIDDEN_FILENAMES: &[&str] = &[
    ".env",
];

/// Glob prefixes of `.env.*` files (e.g. `.env.local`, `.env.production`).
const FORBIDDEN_FILENAME_PREFIXES: &[&str] = &[
    ".env.",
];

/// Path prefixes (in security context) that scan-exempt rules may not match.
/// Used to block attempts to exempt `.ssh/`, `.gnupg/`, etc.
const FORBIDDEN_SCAN_EXEMPT_PREFIXES: &[&str] = &[
    ".ssh/",
    ".ssh",
    ".gnupg/",
    ".gnupg",
    ".git/",
    ".git",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Check whether a repo-relative forward-slash path is security-forbidden.
///
/// Returns `true` iff the path must NEVER be indexed regardless of any rule.
pub fn is_security_forbidden(repo_relative_path: &str) -> bool {
    let path = repo_relative_path;

    // --- Forbidden directory segments anywhere in path ---
    for component in path.split('/') {
        if FORBIDDEN_DIRS.contains(&component) {
            return true;
        }
    }

    // --- File name checks (last component) ---
    let file_name = path.split('/').next_back().unwrap_or(path);
    let file_name_lower = file_name.to_ascii_lowercase();

    // Exact filename match
    if FORBIDDEN_FILENAMES.contains(&file_name_lower.as_str()) {
        return true;
    }

    // Prefix match (.env.*)
    for prefix in FORBIDDEN_FILENAME_PREFIXES {
        if file_name_lower.starts_with(prefix) {
            return true;
        }
    }

    // Extension check
    if let Some(ext) = extension_of(file_name)
        && FORBIDDEN_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
    {
        return true;
    }

    false
}

/// Assert that a scan-exempt path does not overlap any security-forbidden prefix.
/// Called from `DiscoveryPolicy::validate()`.
pub fn assert_not_forbidden_prefix(path: &str) -> Result<(), DiscoveryError> {
    let normalized = path.trim_start_matches('/').to_ascii_lowercase();
    for forbidden in FORBIDDEN_SCAN_EXEMPT_PREFIXES {
        if normalized == forbidden.trim_end_matches('/')
            || normalized.starts_with(forbidden)
        {
            return Err(DiscoveryError::ScanExemptForbiddenPath(path.to_owned()));
        }
    }
    // Also check individual path components for forbidden dir names
    for component in normalized.split('/') {
        if FORBIDDEN_DIRS.contains(&component) {
            return Err(DiscoveryError::ScanExemptForbiddenPath(path.to_owned()));
        }
    }
    Ok(())
}

/// Validate that `canonical_path` is strictly within `allowed_root`.
///
/// Both paths must already be canonicalized (i.e., no symlinks, no `..`).
/// Returns `Ok(())` if safe, or `Err(DiscoveryError::PathEscape)` if not.
pub fn assert_within_root(
    canonical_path: &Path,
    canonical_root: &Path,
) -> Result<(), DiscoveryError> {
    if canonical_path.starts_with(canonical_root) {
        Ok(())
    } else {
        Err(DiscoveryError::PathEscape(canonical_path.to_owned()))
    }
}

/// Canonicalize `path`, then verify it stays within `allowed_root`.
///
/// On success returns the canonical `PathBuf`.
/// On symlink escape returns `Err(DiscoveryError::PathEscape)`.
/// On I/O error (e.g. path does not exist) returns `Err(DiscoveryError::Canonicalize)`.
pub fn canonicalize_within_root(
    path: &Path,
    allowed_root: &Path,
) -> Result<PathBuf, DiscoveryError> {
    let canonical = std::fs::canonicalize(path).map_err(|e| DiscoveryError::Canonicalize {
        path: path.to_owned(),
        source: e,
    })?;
    assert_within_root(&canonical, allowed_root)?;
    Ok(canonical)
}

/// Normalize a repo-relative path to use forward slashes.
///
/// On Windows `\\` separators are converted to `/`.
/// Leading slashes and `.`/`..` components cause `None` (treated as INACCESSIBLE).
pub fn normalize_repo_relative(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    // Reject absolute paths and path traversal
    if rel.is_absolute() {
        return None;
    }
    let mut parts = Vec::new();
    for component in rel.components() {
        use std::path::Component;
        match component {
            Component::Normal(s) => {
                let s = s.to_str()?;
                // Reject null bytes in path components
                if s.contains('\0') {
                    return None;
                }
                parts.push(s.to_owned());
            }
            // Reject traversal
            Component::ParentDir | Component::CurDir | Component::Prefix(_) => return None,
            Component::RootDir => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

fn extension_of(file_name: &str) -> Option<&str> {
    // Find the last '.' that is not the first character
    let bytes = file_name.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'.' {
            return Some(&file_name[i + 1..]);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_dir_is_forbidden() {
        assert!(is_security_forbidden(".git/config"));
        assert!(is_security_forbidden(".git/objects/ab/cd"));
        assert!(is_security_forbidden("subdir/.git/config"));
    }

    #[test]
    fn ssh_dir_is_forbidden() {
        assert!(is_security_forbidden(".ssh/id_rsa"));
        assert!(is_security_forbidden("home/.ssh/known_hosts"));
    }

    #[test]
    fn gnupg_dir_is_forbidden() {
        assert!(is_security_forbidden(".gnupg/pubring.gpg"));
    }

    #[test]
    fn pem_extension_is_forbidden() {
        assert!(is_security_forbidden("certs/server.pem"));
        assert!(is_security_forbidden("ROOT.PEM")); // case-insensitive
    }

    #[test]
    fn key_extension_is_forbidden() {
        assert!(is_security_forbidden("secrets/private.key"));
    }

    #[test]
    fn p12_jks_forbidden() {
        assert!(is_security_forbidden("keystore.p12"));
        assert!(is_security_forbidden("keystore.jks"));
    }

    #[test]
    fn dotenv_is_forbidden() {
        assert!(is_security_forbidden(".env"));
        assert!(is_security_forbidden("backend/.env"));
        assert!(is_security_forbidden(".env.local"));
        assert!(is_security_forbidden(".env.production"));
    }

    #[test]
    fn normal_files_are_not_forbidden() {
        assert!(!is_security_forbidden("src/main.rs"));
        assert!(!is_security_forbidden("README.md"));
        assert!(!is_security_forbidden("tests/integration.rs"));
        assert!(!is_security_forbidden("vendor/lib/foo.js"));
        assert!(!is_security_forbidden("migrations/0001_initial.sql"));
    }

    #[test]
    fn env_example_is_not_forbidden() {
        // .env.example is a common pattern for template files; it does NOT
        // start with ".env." followed by a real environment name.
        // However, per the contract, .env.* is forbidden (conservative approach).
        // This test verifies the conservative behaviour.
        assert!(is_security_forbidden(".env.example"));
    }

    #[test]
    fn scan_exempt_ssh_rejected() {
        assert!(assert_not_forbidden_prefix(".ssh/known_hosts").is_err());
        assert!(assert_not_forbidden_prefix(".ssh").is_err());
        assert!(assert_not_forbidden_prefix(".gnupg").is_err());
        assert!(assert_not_forbidden_prefix(".git").is_err());
    }

    #[test]
    fn scan_exempt_normal_path_allowed() {
        assert!(assert_not_forbidden_prefix("fixtures/example_keys").is_ok());
        assert!(assert_not_forbidden_prefix("tests/secrets_fixtures").is_ok());
    }

    #[test]
    fn path_within_root_ok() {
        use std::path::PathBuf;
        let root = PathBuf::from("/workspace/project");
        let child = PathBuf::from("/workspace/project/src/main.rs");
        assert!(assert_within_root(&child, &root).is_ok());
    }

    #[test]
    fn path_outside_root_rejected() {
        use std::path::PathBuf;
        let root = PathBuf::from("/workspace/project");
        let escape = PathBuf::from("/workspace/other/file.rs");
        assert!(assert_within_root(&escape, &root).is_err());
    }

    #[test]
    fn normalize_repo_relative_basic() {
        use std::path::PathBuf;
        let root = PathBuf::from("/workspace/project");
        let file = PathBuf::from("/workspace/project/src/main.rs");
        assert_eq!(
            normalize_repo_relative(&file, &root).unwrap(),
            "src/main.rs"
        );
    }

    #[test]
    fn normalize_repo_relative_traversal_rejected() {
        use std::path::PathBuf;
        // Build a path with ParentDir component using join
        let root = PathBuf::from("/workspace/project");
        let traversal = root.join("..").join("other").join("secret.rs");
        // normalize_repo_relative should return None since strip_prefix will
        // fail if the path was not yet resolved.
        assert!(normalize_repo_relative(&traversal, &root).is_none());
    }
}
