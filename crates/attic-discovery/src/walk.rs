//! Repository walking: converts a configured root into an ordered list of
//! [`EligibleEntry`] values using the `ignore` crate for gitignore-aware
//! traversal and the security / classification layers for filtering.
//!
//! # Walk precedence (implemented order)
//!
//! 1. **Security exclusions** — ABSOLUTE, cannot be overridden by any rule.
//! 2. **Submodule / nested-repo boundaries** — detected and emitted as
//!    [`DiagnosticKind::SubmoduleDetected`]; the subtree is not descended.
//! 3. **`include_untracked = false`** — when `git_aware = true` and this flag
//!    is `false`, only files returned by `git ls-files --cached` are eligible
//!    (tracked files).  Files that explicitly match an `attic_include_rule`
//!    are also admitted regardless of tracked status.
//! 4. **Git-ignore evaluation** — applied by the `ignore` crate walker.
//! 5. **Attic include rules override gitignore** — if `attic_include_rules` is
//!    non-empty, a second gitignore-disabled walk is performed and any file
//!    matching an include rule is added (security exclusions still apply).
//! 6. **Default exclusions** — applied via [`classification::classify`].
//! 7. **Attic exclude rules** — applied via [`classification::classify`].
//! 8. **Priority classification** — applied via [`classification::classify`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use ignore::WalkBuilder;

use crate::{
    classification::{classify, glob_matches},
    diagnostics::{Diagnostic, DiagnosticKind, WalkCounters},
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
    /// Explainability counters accumulated across every pass of this walk.
    pub counters: WalkCounters,
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
    walk_with_cancellation(root, policy, &attic_core::CancellationToken::default())
}

/// Cancellable repository walk.
pub fn walk_with_cancellation(
    root: &Path,
    policy: &DiscoveryPolicy,
    cancellation: &attic_core::CancellationToken,
) -> Result<WalkResult, DiscoveryError> {
    if !root.is_dir() {
        return Err(DiscoveryError::RootNotDirectory(root.to_path_buf()));
    }

    let mut result = WalkResult::default();

    // ── Step 1: obtain tracked-file set if include_untracked = false ───────
    // FAIL CLOSED: if the caller explicitly requested include_untracked=false
    // and we cannot obtain the Git tracked-file set, we must NOT silently
    // fall back to include_untracked=true semantics.  Broadening the discovery
    // scope without authorisation violates the policy contract.
    let tracked_files: Option<HashSet<String>> = if policy.git_aware && !policy.include_untracked {
        let set =
            git_tracked_files(root).map_err(|e| DiscoveryError::TrackedFileSetUnavailable {
                reason: e.to_string(),
            })?;
        Some(set)
    } else {
        None
    };

    // ── Step 2: main walk (gitignore-aware) ────────────────────────────────
    // Shared across every pass below so a directory/file re-visited by a
    // later pass (Step 3/4 disable gitignore and re-walk the same tree) is
    // never counted into `result.counters` more than once.
    let mut seen = SeenPaths::default();
    let mut main_entries: HashSet<String> = HashSet::new();
    walk_pass(
        root,
        policy,
        /*respect_gitignore=*/ policy.git_aware,
        &tracked_files,
        &mut result,
        &mut seen,
        cancellation,
        &mut |entry| {
            main_entries.insert(entry.repo_relative.clone());
            Some(entry)
        },
    )?;

    // ── Step 3: tracked-but-gitignored files (include_untracked = false) ──
    // When include_untracked=false, a tracked file that is gitignored will be
    // pruned by the ignore-crate walker in Pass 1 before the tracked-file
    // filter can rescue it.  A second gitignore-disabled pass is therefore
    // required whenever the tracked-file set is non-empty so those files are
    // still surfaced.  Security exclusions continue to apply.
    if policy.git_aware
        && !policy.include_untracked
        && let Some(tracked) = &tracked_files
        && !tracked.is_empty()
    {
        walk_pass(
            root,
            policy,
            /*respect_gitignore=*/ false,
            &tracked_files, // tracked-file filter still applied inside walk_pass
            &mut result,
            &mut seen,
            cancellation,
            &mut |entry| {
                if !main_entries.contains(&entry.repo_relative) {
                    main_entries.insert(entry.repo_relative.clone());
                    Some(entry)
                } else {
                    None
                }
            },
        )?;
    }

    // ── Step 4: Attic-include override of gitignored paths ─────────────────
    // If any attic_include_rule exists, do a further walk with gitignore
    // disabled.  Only paths matching an include rule (and not already found
    // in earlier passes) are considered.  Security exclusions still apply.
    if policy.git_aware && !policy.attic_include_rules.is_empty() {
        walk_pass(
            root,
            policy,
            /*respect_gitignore=*/ false,
            &tracked_files,
            &mut result,
            &mut seen,
            cancellation,
            &mut |entry| {
                // Only accept if an attic_include_rule matches this path
                // and the path was NOT already captured in an earlier pass.
                let explicitly_included = policy
                    .attic_include_rules
                    .iter()
                    .any(|rule| glob_matches(&rule.pattern, &entry.repo_relative));
                if explicitly_included && !main_entries.contains(&entry.repo_relative) {
                    main_entries.insert(entry.repo_relative.clone());
                    Some(entry)
                } else {
                    None
                }
            },
        )?;
    }

    // Sort for determinism: repo-relative path, lexicographic.
    result
        .entries
        .sort_by(|a, b| a.repo_relative.cmp(&b.repo_relative));

    // Dedup across passes happens in each pass's `filter` closure (only a
    // genuinely new path is ever pushed), so the final entry count is
    // exactly the eligible-file count with no double-counting.
    result.counters.files_eligible = result.entries.len() as u64;

    Ok(result)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Absolute paths already counted into `WalkResult::counters`, shared
/// across every pass of one `walk()` call so a directory/file re-visited by
/// a later pass is never double-counted (bundled into one struct to keep
/// `walk_pass`'s argument count down).
#[derive(Default)]
struct SeenPaths {
    dirs: HashSet<PathBuf>,
    files: HashSet<PathBuf>,
}

/// One walk pass over `root`.  The `filter` closure can transform, accept
/// (`Some(entry)`) or reject (`None`) each candidate entry.
///
/// When `filter` is `&mut |entry| entry` (i.e. returns the entry unchanged)
/// use the `&mut |entry| { ...; entry }` form.
#[allow(clippy::too_many_arguments)]
fn walk_pass<F>(
    root: &Path,
    policy: &DiscoveryPolicy,
    respect_gitignore: bool,
    tracked_files: &Option<HashSet<String>>,
    result: &mut WalkResult,
    seen: &mut SeenPaths,
    cancellation: &attic_core::CancellationToken,
    filter: &mut F,
) -> Result<(), DiscoveryError>
where
    F: FnMut(EligibleEntry) -> Option<EligibleEntry>,
{
    let SeenPaths {
        dirs: seen_dirs,
        files: seen_files,
    } = seen;
    let mut builder = WalkBuilder::new(root);

    builder
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore && policy.global_gitconfig_excludes)
        .git_exclude(respect_gitignore)
        .ignore(false)
        .hidden(true)
        .follow_links(false);

    builder.threads(1);

    // Track submodule root prefixes discovered during the walk so we can
    // skip their contents.
    let mut submodule_prefixes: Vec<String> = Vec::new();

    for entry_result in builder.build() {
        if cancellation.is_cancelled() {
            return Err(DiscoveryError::Cancelled);
        }
        match entry_result {
            Err(walk_err) => {
                result.counters.transient_failures += 1;
                result.diagnostics.push(Diagnostic {
                    kind: DiagnosticKind::IoError,
                    path: PathBuf::new(),
                    message: walk_err.to_string(),
                });
            }
            Ok(dent) => {
                // Skip the root directory entry itself.
                if dent.depth() == 0 {
                    continue;
                }

                let abs_path = dent.path();

                // ── Submodule boundary detection ───────────────────────────
                // A non-root directory is a submodule root if it contains a
                // `.git` file (detached submodule) or `.git/` directory.
                //
                // `walk()` can run this pass up to three times over the same
                // root (gitignore-respecting, tracked-but-gitignored,
                // attic-include-override). `seen_dirs`/`seen_files` are
                // shared across all of those calls so a directory/file
                // re-visited by a later pass is never counted twice — the
                // submodule_prefixes rebuild below still runs every pass
                // (it's pass-local and needed for this pass's own filtering
                // decisions), only the *counters* are deduplicated.
                if dent.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                    let is_new_dir = seen_dirs.insert(abs_path.to_path_buf());
                    if is_new_dir {
                        result.counters.directories_visited += 1;
                    }
                    let has_dot_git = abs_path.join(".git").exists();
                    if has_dot_git && let Some(rel) = normalize_repo_relative(abs_path, root) {
                        if is_new_dir {
                            result.counters.nested_repo_boundaries += 1;
                            result.diagnostics.push(Diagnostic {
                                kind: DiagnosticKind::SubmoduleDetected,
                                path: abs_path.to_path_buf(),
                                message: format!(
                                    "submodule/nested repository detected at '{rel}'; treating as separate repository"
                                ),
                            });
                        }
                        submodule_prefixes.push(rel + "/");
                    }
                    continue; // never index directory entries themselves
                }

                let is_new_file = seen_files.insert(abs_path.to_path_buf());
                if is_new_file {
                    result.counters.files_seen += 1;
                }

                // ── Skip files under detected submodule roots ──────────────
                let repo_rel = match normalize_repo_relative(abs_path, root) {
                    Some(r) => r,
                    None => continue,
                };

                if submodule_prefixes
                    .iter()
                    .any(|pfx| repo_rel.starts_with(pfx.as_str()))
                {
                    if is_new_file {
                        result.counters.ignored_or_pruned += 1;
                    }
                    continue;
                }

                // ── Symlink handling ───────────────────────────────────────
                if dent.path_is_symlink() {
                    match abs_path.canonicalize() {
                        Err(_) => {
                            if is_new_file {
                                result.counters.symlinks_skipped += 1;
                                result.diagnostics.push(Diagnostic {
                                    kind: DiagnosticKind::SymlinkCycle,
                                    path: abs_path.to_path_buf(),
                                    message: "symlink target unresolvable".into(),
                                });
                            }
                            continue;
                        }
                        Ok(canonical) => {
                            if let Err(_e) = assert_within_root(&canonical, root) {
                                if is_new_file {
                                    result.counters.symlinks_skipped += 1;
                                    result.diagnostics.push(Diagnostic {
                                        kind: DiagnosticKind::SymlinkEscape,
                                        path: abs_path.to_path_buf(),
                                        message: format!(
                                            "symlink escapes root: {} -> {}",
                                            abs_path.display(),
                                            canonical.display()
                                        ),
                                    });
                                }
                                continue;
                            }
                        }
                    }
                }

                // Only process regular files.
                if !dent.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
                    if is_new_file {
                        result.counters.ignored_or_pruned += 1;
                    }
                    continue;
                }

                // ── Security exclusions (ABSOLUTE) ─────────────────────────
                if is_security_forbidden(&repo_rel) {
                    if is_new_file {
                        result.counters.security_exclusions += 1;
                    }
                    continue;
                }

                // ── include_untracked = false ──────────────────────────────
                // When a tracked-file set is available, admit only tracked
                // files.  Files explicitly matching an attic_include_rule are
                // also admitted regardless of tracked status (the user has
                // made an explicit decision to include them).
                if let Some(tracked) = tracked_files {
                    let is_tracked = tracked.contains(&repo_rel);
                    let is_explicitly_included = policy
                        .attic_include_rules
                        .iter()
                        .any(|rule| glob_matches(&rule.pattern, &repo_rel));
                    if !is_tracked && !is_explicitly_included {
                        if is_new_file {
                            result.counters.ignored_or_pruned += 1;
                        }
                        continue;
                    }
                }

                // ── Apply policy default exclusions + classification ────────
                let priority = classify(&repo_rel, policy);
                if priority == DiscoveryPriority::Ignored {
                    if is_new_file {
                        result.counters.ignored_or_pruned += 1;
                    }
                    continue;
                }

                let candidate = EligibleEntry {
                    abs_path: abs_path.to_path_buf(),
                    repo_relative: repo_rel,
                    priority,
                };

                if let Some(accepted) = filter(candidate) {
                    result.entries.push(accepted);
                }
            }
        }
    }

    Ok(())
}

/// Invoke `git ls-files --cached --full-name -z` in `repo_root` to obtain
/// the set of paths tracked by Git.
///
/// Returns a `HashSet` of repo-relative paths using forward-slash separators.
/// Returns an error if the `git` process cannot be spawned or exits non-zero.
pub fn git_tracked_files(repo_root: &Path) -> Result<HashSet<String>, std::io::Error> {
    let output = Command::new("git")
        .args(["ls-files", "--cached", "--full-name", "-z"])
        .current_dir(repo_root)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "git ls-files exited with {}: {stderr}",
            output.status
        )));
    }

    // `-z` output: NUL-separated paths, no trailing newline on last item.
    let mut set = HashSet::new();
    for path in output.stdout.split(|&b| b == 0) {
        if path.is_empty() {
            continue;
        }
        if let Ok(s) = std::str::from_utf8(path) {
            // Normalise Windows back-slashes just in case.
            set.insert(s.replace('\\', "/"));
        }
    }
    Ok(set)
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

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        assert!(
            paths.contains(&"src/main.rs"),
            "expected src/main.rs in {paths:?}"
        );
        assert!(paths.contains(&"src/lib.rs"));

        // Explainability counters (PR-3): every eligible file is both
        // "seen" and "eligible" when nothing excludes it.
        assert_eq!(result.counters.files_eligible, result.entries.len() as u64);
        assert_eq!(result.counters.files_seen, 2);
        assert!(
            result.counters.directories_visited >= 1,
            "src/ itself must be visited"
        );
        assert_eq!(result.counters.ignored_or_pruned, 0);
        assert_eq!(result.counters.security_exclusions, 0);
    }

    #[test]
    fn counters_distinguish_ignored_from_eligible() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/keep.rs", "");
        write(root, "node_modules/pkg/index.js", "");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        assert_eq!(result.counters.files_eligible, 1, "only src/keep.rs");
        assert!(
            result.counters.files_seen >= 2,
            "both files must be seen before exclusion: {:?}",
            result.counters
        );
        assert!(
            result.counters.ignored_or_pruned >= 1,
            "node_modules/pkg/index.js must be counted as pruned: {:?}",
            result.counters
        );
    }

    #[test]
    fn counters_count_security_exclusion_separately_from_ignored() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "");
        write(root, "certs/server.pem", "-----BEGIN CERTIFICATE-----");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        assert_eq!(result.counters.files_eligible, 1);
        assert_eq!(
            result.counters.security_exclusions, 1,
            "server.pem must be counted as a security exclusion, not a generic prune: {:?}",
            result.counters
        );
    }

    #[test]
    fn counters_count_nested_repo_boundary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/root_file.rs", "fn root() {}");
        setup_git_repo(&root.join("vendor/child"));
        write(root, "vendor/child/lib.rs", "fn nested() {}");

        let policy = DiscoveryPolicy::default_git();
        let result = walk(root, &policy).unwrap();

        assert_eq!(
            result.counters.nested_repo_boundaries, 1,
            "vendor/child must be counted as one detected boundary: {:?}",
            result.counters
        );
        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        assert!(!paths.iter().any(|p| p.starts_with("vendor/child/")));
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

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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

        let any_git = result
            .entries
            .iter()
            .any(|e| e.repo_relative.starts_with(".git/"));
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

        let pem = result
            .entries
            .iter()
            .any(|e| e.repo_relative.ends_with(".pem"));
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

        let has_env = result
            .entries
            .iter()
            .any(|e| e.repo_relative == ".env" || e.repo_relative.starts_with(".env."));
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

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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
        policy.git_aware = false;
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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
        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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
        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        assert!(paths.contains(&"src/keep.rs"));
        assert!(
            !paths.contains(&"src/drop.rs"),
            "explicit exclude should drop src/drop.rs"
        );
    }

    // ── New tests required by review ──────────────────────────────────────

    /// When `include_untracked = false` and `git ls-files` fails (no git repo,
    /// no commits), the walk must return an error — not silently fall back to
    /// `include_untracked = true` semantics.
    #[test]
    fn include_untracked_false_fails_closed_when_git_unavailable() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Deliberately NOT a valid git repo (no `git init`), so `git ls-files`
        // will fail.  A bare `.git/` stub is also insufficient for ls-files.
        // We create just normal files — no git machinery.
        write(root, "src/main.rs", "fn main() {}");

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = false;

        let result = walk(root, &policy);
        assert!(
            result.is_err(),
            "walk must fail when include_untracked=false and git tracked-file set is unavailable"
        );
        match result.unwrap_err() {
            crate::error::DiscoveryError::TrackedFileSetUnavailable { .. } => {}
            other => panic!("expected TrackedFileSetUnavailable, got {other:?}"),
        }
    }

    /// `include_untracked = false` with a real `git init` + `git add`:
    /// tracked files appear, untracked files are suppressed.
    #[test]
    fn include_untracked_false_excludes_untracked_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Full git init so git ls-files works.
        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root)
            .status();
        // If git is not available in CI, skip.
        if status.is_err() || !status.unwrap().success() {
            return;
        }

        write(root, "src/tracked.rs", "fn t() {}");
        write(root, "src/untracked.rs", "fn u() {}");

        // Stage only tracked.rs
        let _ = Command::new("git")
            .args(["add", "src/tracked.rs"])
            .current_dir(root)
            .status();

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = false;
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        assert!(
            paths.contains(&"src/tracked.rs"),
            "tracked file must appear: {paths:?}"
        );
        assert!(
            !paths.contains(&"src/untracked.rs"),
            "untracked file must be excluded: {paths:?}"
        );
    }

    /// A tracked file that happens to match a `.gitignore` rule is still
    /// returned when `include_untracked = false` (tracked takes priority).
    #[test]
    fn tracked_file_matching_gitignore_still_returned() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let status = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(root)
            .status();
        if status.is_err() || !status.unwrap().success() {
            return;
        }

        // .gitignore ignores generated/
        write(root, ".gitignore", "generated/\n");
        write(root, "src/lib.rs", "");
        write(root, "generated/proto.rs", "// generated");

        // Track both files (git add -f to force-add the ignored one).
        let _ = Command::new("git")
            .args(["add", "src/lib.rs"])
            .current_dir(root)
            .status();
        let _ = Command::new("git")
            .args(["add", "-f", "generated/proto.rs"])
            .current_dir(root)
            .status();

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = false;
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        // generated/proto.rs is tracked but gitignored; with include_untracked=false
        // it must still appear because it is tracked.
        // The `ignore` crate gitignore prunes the directory in Pass 1, but the
        // tracked-file set causes a second gitignore-disabled pass (Pass 2)
        // where the file IS found because include_untracked_rules admits all
        // tracked paths in Pass 2 when no explicit attic_include_rules exist.
        //
        // If git is not available this test was already skipped above.
        assert!(
            paths.contains(&"src/lib.rs"),
            "src/lib.rs should be returned: {paths:?}"
        );
        // generated/proto.rs is tracked so it should survive even if gitignored.
        assert!(
            paths.contains(&"generated/proto.rs"),
            "tracked-but-gitignored file should appear: {paths:?}"
        );
    }

    /// A gitignored directory explicitly re-included by an Attic include rule
    /// must appear in the output (Pass 2 override), even with no git repo.
    #[test]
    fn gitignored_dir_explicitly_reincluded_by_attic() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // gitignore hides the whole "private/" directory
        write(root, ".gitignore", "private/\n");
        write(root, "private/data.rs", "// private");
        write(root, "src/lib.rs", "// public");

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = true;
        // Attic include rule explicitly re-includes the gitignored sub-path.
        policy.attic_include_rules.push(crate::policy::GlobRule {
            pattern: "private/data.rs".to_string(),
            negation: false,
        });

        let result = walk(root, &policy).unwrap();
        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();

        assert!(
            paths.contains(&"private/data.rs"),
            "gitignored file re-included by attic rule should appear; got {paths:?}"
        );
        assert!(
            paths.contains(&"src/lib.rs"),
            "src/lib.rs should also be present"
        );
    }

    /// Code-review finding: `walk()` can invoke `walk_pass` up to three
    /// times over the same root; explainability counters must not multiply
    /// with the number of passes. Compares a single-pass run against a
    /// multi-pass run over the *identical* fixture (the second pass' extra
    /// include rule matches a file already found by pass one, so it adds
    /// nothing new — only triggers the extra full re-walk).
    #[test]
    fn multi_pass_walk_does_not_multiply_explainability_counters() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write(root, "src/main.rs", "fn main() {}");
        write(root, "src/lib.rs", "pub fn foo() {}");

        let single_pass_policy = DiscoveryPolicy::default_git();
        let single = walk(root, &single_pass_policy).unwrap();

        let mut multi_pass_policy = DiscoveryPolicy::default_git();
        multi_pass_policy
            .attic_include_rules
            .push(crate::policy::GlobRule {
                pattern: "src/main.rs".to_string(),
                negation: false,
            });
        let multi = walk(root, &multi_pass_policy).unwrap();

        assert_eq!(
            single.entries.len(),
            multi.entries.len(),
            "same eligible files either way"
        );
        assert_eq!(
            single.counters.files_seen, multi.counters.files_seen,
            "a redundant second pass must not double-count files_seen"
        );
        assert_eq!(
            single.counters.directories_visited, multi.counters.directories_visited,
            "a redundant second pass must not double-count directories_visited"
        );
        assert_eq!(
            single.counters.files_eligible, multi.counters.files_eligible,
            "files_eligible must match regardless of pass count"
        );
    }

    /// A security-forbidden path must never appear even when an Attic include
    /// rule explicitly references it.
    #[test]
    fn security_forbidden_path_cannot_be_reincluded() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        write(root, ".env", "PASSWORD=hunter2");
        write(root, "src/safe.rs", "fn ok() {}");

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = true;
        // Attempt to force-include the forbidden path via an include rule.
        policy.attic_include_rules.push(crate::policy::GlobRule {
            pattern: ".env".to_string(),
            negation: false,
        });

        let result = walk(root, &policy).unwrap();
        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();

        assert!(
            !paths.contains(&".env"),
            ".env must never appear even with an include rule; got {paths:?}"
        );
        assert!(paths.contains(&"src/safe.rs"));
    }

    /// A nested directory containing a `.git` entry must be detected as a
    /// submodule boundary.  Files inside it must be absent; a
    /// `SubmoduleDetected` diagnostic must be emitted.
    #[test]
    fn nested_repo_detected_as_submodule() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        write(root, "src/root_file.rs", "fn root() {}");

        // Simulate a submodule: a subdirectory that contains a `.git` directory.
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        let sub_git = sub.join(".git");
        fs::create_dir_all(&sub_git).unwrap();
        fs::write(sub_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        write(root, "sub/sub_file.rs", "fn sub() {}");

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = true;
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();

        // The submodule's file must NOT be in the output.
        assert!(
            !paths.contains(&"sub/sub_file.rs"),
            "file inside submodule must be excluded; got {paths:?}"
        );

        // The root file must be present.
        assert!(
            paths.contains(&"src/root_file.rs"),
            "root_file.rs should be present; got {paths:?}"
        );

        // A SubmoduleDetected diagnostic must have been emitted.
        let has_diag = result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::SubmoduleDetected);
        assert!(
            has_diag,
            "SubmoduleDetected diagnostic expected; got {:?}",
            result.diagnostics
        );
    }

    /// A `.git` *file* (worktree / real submodule checkout form) is also a
    /// submodule boundary.
    #[test]
    fn nested_repo_with_git_file_detected_as_submodule() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        write(root, "root.rs", "fn r() {}");

        let wt = root.join("worktree");
        fs::create_dir_all(&wt).unwrap();
        // `.git` as a *file* — worktree / submodule checkout form
        fs::write(wt.join(".git"), "gitdir: ../.git/worktrees/wt\n").unwrap();
        write(root, "worktree/wt_file.rs", "fn wt() {}");

        let mut policy = DiscoveryPolicy::default_git();
        policy.include_untracked = true;
        let result = walk(root, &policy).unwrap();

        let paths: Vec<&str> = result
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();

        assert!(
            !paths.contains(&"worktree/wt_file.rs"),
            "file inside git-file submodule must be excluded; got {paths:?}"
        );
        let has_diag = result
            .diagnostics
            .iter()
            .any(|d| d.kind == DiagnosticKind::SubmoduleDetected);
        assert!(
            has_diag,
            "SubmoduleDetected diagnostic expected for worktree; got {:?}",
            result.diagnostics
        );
    }
}
