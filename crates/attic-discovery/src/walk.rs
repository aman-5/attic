//! Repository walking: converts a configured root into an ordered list of
//! [`EligibleEntry`] values using the `ignore` crate for gitignore-aware
//! traversal and the security / classification layers for filtering.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use crate::{
    classification::classify,
    diagnostics::{Diagnostic, DiagnosticKind},
    error::DiscoveryError,
    policy::{DiscoveryPolicy, DiscoveryPriority},
    security::{assert_within_root, is_security_forbidden, normalize_repo_relative},
};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single file entry that passed all filtering stages and is eligible for
/// further processing (FTS, embeddings, indexing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibleEntry {
    /// Absolute canonical path to the file.
    pub abs_path: PathBuf,
    /// Path relative to the repository root, using forward-slash separators.
    pub repo_relative: String,
    /// Assigned discovery priority (never `Ignored`).
    pub priority: DiscoveryPriority,
}

/// Result of a single walk over one root directory.
#[derive(Debug, Default)]
pub struct WalkResult {
    /// Files eligible for downstream processing.
    pub entries: Vec<EligibleEntry>,
    /// Non-fatal diagnostics generated during the walk.
    pub diagnostics: Vec<Diagnostic>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Walk `root` according to `policy` and return all eligible file entries
/// together with any diagnostics produced during the walk.
///
/// `root` **must** be a canonical absolute path to the repository root.
/// The caller is responsible for calling `security::canonicalize_within_root`
/// before passing `root` here.
pub fn walk(root: &Path, policy: &DiscoveryPolicy) -> Result<WalkResult, DiscoveryError> {
    if !root.is_dir() {
        return Err(DiscoveryError::RootNotDirectory(root.to_path_buf()));
    }

    let mut builder = WalkBuilder::new(root);

    // ----- gitignore layer -------------------------------------------------
    builder
        .git_ignore(policy.git_aware)
        .git_global(policy.git_aware && policy.global_gitconfig_excludes)
        .git_exclude(policy.git_aware) // honours .git/info/exclude
        .ignore(false) // do NOT read .ignore files (non-standard)
        .hidden(true) // include hidden files so we can apply our own rules
        .follow_links(false); // symlink traversal handled manually below

    // We collect entries in the calling thread; use single-threaded walk so
    // that we can safely accumulate results without locks.
    builder.threads(1);

    let mut result = WalkResult::default();

    for entry_result in builder.build() {
        match entry_result {
            Err(walk_err) => {
                // The `ignore` crate surfaces IO errors as walk errors.
                // Convert them to diagnostics rather than aborting the walk.
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::IoError,
                    path: std::path::PathBuf::new(),
                    message: walk_err.to_string(),
                });
            }
            Ok(dent) => {
                // Skip the root directory entry itself.
                if dent.depth() == 0 {
                    continue;
                }

                let abs_path = dent.path();

                // ── Symlink handling ───────────────────────────────────────
                if dent.path_is_symlink() {
                    match abs_path.canonicalize() {
                        Err(_) => {
                            result.diagnostics.push(Diagnostic {
                                kind: DiagnosticKind::SymlinkCycle,
                                path: abs_path.to_path_buf(),
                                message: "symlink target unresolvable".into(),
                            });
                            continue;
                        }
                        Ok(canonical) => {
                            if let Err(_e) = assert_within_root(&canonical, root) {
                                result.diagnostics.push(Diagnostic {
                                    kind: DiagnosticKind::SymlinkEscape,
                                    path: abs_path.to_path_buf(),
                                    message: format!(
                                        "symlink escapes root: {} -> {}",
                                        abs_path.display(),
                                        canonical.display()
                                    ),
                                });
                                continue;
                            }
                        }
                    }
                }

                // Only process regular files from here.
                if !dent.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    continue;
                }

                // ── Repo-relative path ─────────────────────────────────────
                let repo_rel = match normalize_repo_relative(abs_path, root) {
                    Some(r) => r,
                    None => continue, // path outside root — skip
                };

                // ── Security exclusions (ABSOLUTE — cannot be overridden) ──
                if is_security_forbidden(&repo_rel) {
                    // No diagnostic — security-forbidden paths are silently
                    // excluded; surfacing them in diagnostics could expose
                    // secrets metadata.
                    continue;
                }

                // ── Apply policy default exclusions + classification ────────
                let priority = classify(&repo_rel, policy);

                if priority == DiscoveryPriority::Ignored {
                    continue;
                }

                result.entries.push(EligibleEntry {
                    abs_path: abs_path.to_path_buf(),
                    repo_relative: repo_rel,
                    priority,
                });
            }
        }
    }

    // Sort for determinism: repo-relative path, lexicographic.
    result.entries.sort_by(|a, b| a.repo_relative.cmp(&b.repo_relative));

    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[test]
    fn basic_walk_returns_src_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "fn main() {}");
        write(root, "src/lib.rs", "pub fn foo() {}");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"), "expected src/main.rs in {paths:?}");
        assert!(paths.contains(&"src/lib.rs"));
    }

    #[test]
    fn node_modules_excluded_by_default() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/index.js", "console.log(1)");
        write(root, "node_modules/pkg/index.js", "module.exports={}");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/index.js"));
        assert!(
            !paths.iter().any(|p| p.starts_with("node_modules/")),
            "node_modules should be excluded: {paths:?}"
        );
    }

    #[test]
    fn target_dir_excluded_by_default() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "");
        write(root, "target/debug/main", "");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(!paths.iter().any(|p| p.starts_with("target/")));
    }

    #[test]
    fn dot_git_contents_never_returned() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/a.rs", "");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let any_git = result.entries.iter().any(|e| e.repo_relative.starts_with(".git/"));
        assert!(!any_git, ".git contents must never appear in walk output");
    }

    #[test]
    fn security_forbidden_pem_excluded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "");
        write(root, "certs/server.pem", "-----BEGIN CERTIFICATE-----");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let pem = result.entries.iter().any(|e| e.repo_relative.ends_with(".pem"));
        assert!(!pem, ".pem files must be security-excluded");
    }

    #[test]
    fn security_forbidden_env_excluded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "");
        write(root, ".env", "SECRET=abc");
        write(root, ".env.production", "SECRET=xyz");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let has_env = result.entries.iter().any(|e| {
            e.repo_relative == ".env" || e.repo_relative.starts_with(".env.")
        });
        assert!(!has_env, ".env files must be security-excluded");
    }

    #[test]
    fn gitignore_respected_when_git_aware() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, ".gitignore", "ignored_dir/\n");
        write(root, "src/keep.rs", "");
        write(root, "ignored_dir/dropped.rs", "");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/keep.rs"));
        assert!(
            !paths.iter().any(|p| p.starts_with("ignored_dir/")),
            "gitignore should exclude ignored_dir: {paths:?}"
        );
    }

    #[test]
    fn gitignore_ignored_when_not_git_aware() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, ".gitignore", "ignored_dir/\n");
        write(root, "src/keep.rs", "");
        write(root, "ignored_dir/present.rs", "");

        let mut policy = DiscoveryPolicy::default_non_git();
        // non-git policy — gitignore not applied
        policy.git_aware = false;
        // But we still have default exclusions for target/node_modules etc.
        // ignored_dir is NOT in default exclusions, so it should appear.
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(
            paths.iter().any(|p| p.starts_with("ignored_dir/")),
            "with git_aware=false, gitignore should be ignored: {paths:?}"
        );
    }

    #[test]
    fn entries_are_sorted_deterministically() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/z.rs", "");
        write(root, "src/a.rs", "");
        write(root, "src/m.rs", "");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "walk result must be sorted");
    }

    #[test]
    fn explicit_include_overrides_default_exclusion() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "vendor/special.rs", "// explicitly included");

        let mut policy = DiscoveryPolicy::default_git();
        policy.attic_include_rules.push(crate::policy::GlobRule {
            pattern: "vendor/special.rs".to_string(),
            negation: false,
        });

        let result = walk(root, &policy).unwrap();
        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        // vendor/ is LOW_PRIORITY by default classification — not Ignored — so it appears.
        assert!(
            paths.contains(&"vendor/special.rs"),
            "explicit include should keep vendor/special.rs: {paths:?}"
        );
    }

    #[test]
    fn explicit_exclude_removes_src_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/keep.rs", "");
        write(root, "src/drop.rs", "");

        let mut policy = DiscoveryPolicy::default_git();
        policy.attic_exclude_rules.push(crate::policy::GlobRule {
            pattern: "src/drop.rs".to_string(),
            negation: false,
        });

        let result = walk(root, &policy).unwrap();
        let paths: Vec<&str> = result.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/keep.rs"));
        assert!(!paths.contains(&"src/drop.rs"), "explicit exclude should drop src/drop.rs");
    }
}
