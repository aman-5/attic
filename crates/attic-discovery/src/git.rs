//! Git repository detection and metadata extraction.
//!
//! Reads HEAD SHA and branch name by parsing `.git/HEAD` and
//! `.git/packed-refs` directly.  No `git2` or C FFI dependency is used.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::DiscoveryError;

/// Metadata extracted from a Git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepoMeta {
    /// Absolute canonical path to the repository root (where `.git/` lives).
    pub root: PathBuf,
    /// Current HEAD commit SHA (40 hex chars), if resolvable.
    pub head_sha: Option<String>,
    /// Current branch name (e.g. `main`), `None` when in detached-HEAD state.
    pub branch: Option<String>,
    /// Whether the `.git/HEAD` file contained a valid symbolic ref.
    pub is_detached: bool,
}

/// Detect whether `dir` is the root of a Git repository.
///
/// A directory is considered a Git repository root when it contains a
/// `.git` entry that is either a regular file (worktree) or a directory.
pub fn is_git_root(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Discover Git repository roots below `root`.
///
/// If `root` itself is a Git repository, returns only `root`. Otherwise,
/// recursively finds nested Git repositories without following symlinks or
/// descending into `.git` metadata directories. Results are canonicalized,
/// deduplicated, and sorted lexicographically.
pub fn discover_nested_git_roots(
    root: &Path,
    cancellation: &attic_core::CancellationToken,
) -> Result<Vec<PathBuf>, std::io::Error> {
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    if is_git_root(root) {
        return Ok(vec![root.canonicalize()?]);
    }

    let mut roots = Vec::new();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .ignore(false)
        .hidden(true)
        .follow_links(false)
        .threads(1)
        .filter_entry(|entry| entry.file_name() != std::ffi::OsStr::new(".git"));

    for entry in builder.build() {
        if cancellation.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "repository discovery cancelled",
            ));
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                tracing::warn!("skipping unreadable entry during nested-repo discovery: {e}");
                continue;
            }
        };
        let path = entry.path();
        if entry.depth() > 0 && entry.file_type().is_some_and(|ft| ft.is_dir()) && is_git_root(path)
        {
            roots.push(path.canonicalize()?);
        }
    }

    roots.sort();
    roots.dedup();
    Ok(roots)
}

/// Walk upward from `start` to find the nearest Git repository root.
///
/// Returns `None` when no `.git` is found before hitting the filesystem root.
/// Does **not** cross the `allowed_root` boundary.
pub fn find_git_root(start: &Path, allowed_root: &Path) -> Option<PathBuf> {
    let mut current = start;
    loop {
        if !current.starts_with(allowed_root) {
            return None;
        }
        if is_git_root(current) {
            return Some(current.to_path_buf());
        }
        {
            let parent = current.parent()?;
            current = parent
        }
    }
}

/// Read Git metadata (HEAD SHA + branch) for the repository at `repo_root`.
///
/// Errors are soft: if `.git/HEAD` cannot be read, `head_sha` and `branch`
/// are returned as `None` rather than propagating an error, because the
/// repository may be partially initialised (e.g. immediately after `git init`).
pub fn read_git_meta(repo_root: &Path) -> Result<GitRepoMeta, DiscoveryError> {
    let git_dir = repo_root.join(".git");

    // Handle worktrees: `.git` may be a file containing `gitdir: <path>`
    let git_dir = resolve_git_dir(&git_dir)?;

    let head_path = git_dir.join("HEAD");

    let head_contents = match fs::read_to_string(&head_path) {
        Ok(s) => s,
        Err(_) => {
            // Repository exists but HEAD is unreadable (e.g. just `git init`).
            return Ok(GitRepoMeta {
                root: repo_root.to_path_buf(),
                head_sha: None,
                branch: None,
                is_detached: false,
            });
        }
    };

    let head_contents = head_contents.trim();

    if let Some(ref_path) = head_contents.strip_prefix("ref: ") {
        // Symbolic ref — on a branch.
        let branch = ref_path
            .strip_prefix("refs/heads/")
            .unwrap_or(ref_path)
            .to_string();

        let sha = resolve_ref(ref_path, &git_dir);

        Ok(GitRepoMeta {
            root: repo_root.to_path_buf(),
            head_sha: sha,
            branch: Some(branch),
            is_detached: false,
        })
    } else if looks_like_sha(head_contents) {
        // Detached HEAD — content is the SHA directly.
        Ok(GitRepoMeta {
            root: repo_root.to_path_buf(),
            head_sha: Some(head_contents.to_string()),
            branch: None,
            is_detached: true,
        })
    } else {
        // Unrecognised HEAD format — treat as uninitialised.
        Ok(GitRepoMeta {
            root: repo_root.to_path_buf(),
            head_sha: None,
            branch: None,
            is_detached: false,
        })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Resolve a `.git` file (worktree pointer) or directory to the actual git dir.
fn resolve_git_dir(git_entry: &Path) -> Result<PathBuf, DiscoveryError> {
    if git_entry.is_dir() {
        return Ok(git_entry.to_path_buf());
    }

    if git_entry.is_file() {
        let contents =
            fs::read_to_string(git_entry).map_err(|source| DiscoveryError::Io { source })?;
        let contents = contents.trim();
        if let Some(path) = contents.strip_prefix("gitdir: ") {
            let resolved = git_entry.parent().unwrap_or(Path::new(".")).join(path);
            // Canonicalise so relative `../` segments collapse correctly.
            let canonical =
                fs::canonicalize(&resolved).map_err(|source| DiscoveryError::Canonicalize {
                    path: resolved.clone(),
                    source,
                })?;
            return Ok(canonical);
        }
    }

    // Fall back: return original path even if it does not exist yet.
    Ok(git_entry.to_path_buf())
}

/// Resolve a symbolic ref name (e.g. `refs/heads/main`) to a 40-char SHA.
///
/// Lookup order:
/// 1. Loose ref file: `<git_dir>/refs/heads/<branch>`
/// 2. `packed-refs` file
fn resolve_ref(ref_path: &str, git_dir: &Path) -> Option<String> {
    // 1. Loose ref.
    let loose = git_dir.join(ref_path);
    if let Ok(contents) = fs::read_to_string(&loose) {
        let sha = contents.trim().to_string();
        if looks_like_sha(&sha) {
            return Some(sha);
        }
    }

    // 2. packed-refs.
    let packed = git_dir.join("packed-refs");
    if let Ok(contents) = fs::read_to_string(&packed) {
        for line in contents.lines() {
            // Skip comment lines (start with `#` or `^`).
            if line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            let mut parts = line.splitn(2, ' ');
            if let (Some(sha), Some(name)) = (parts.next(), parts.next())
                && name.trim() == ref_path
                && looks_like_sha(sha)
            {
                return Some(sha.to_string());
            }
        }
    }

    None
}

/// Returns `true` if `s` looks like a 40-character hex SHA-1 or 64-character SHA-256.
fn looks_like_sha(s: &str) -> bool {
    let len = s.len();
    (len == 40 || len == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_bare_repo(dir: &Path) {
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::create_dir_all(dir.join(".git/objects")).unwrap();
    }

    #[test]
    fn is_git_root_detects_dot_git_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        assert!(is_git_root(root));
    }

    #[test]
    fn is_git_root_false_for_plain_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_git_root(tmp.path()));
    }

    #[test]
    fn find_git_root_returns_self_for_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        assert_eq!(find_git_root(root, root), Some(root.to_path_buf()));
    }

    #[test]
    fn find_git_root_walks_upward() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        let deep = root.join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_git_root(&deep, root), Some(root.to_path_buf()));
    }

    #[test]
    fn find_git_root_respects_boundary() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        let sub = root.join("sub");
        fs::create_dir_all(&sub).unwrap();
        // Boundary is sub — should not find repo above it.
        assert_eq!(find_git_root(&sub, &sub), None);
    }

    #[test]
    fn read_git_meta_fresh_repo_no_commits() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        // Write HEAD pointing to a branch that has no commits yet.
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();

        let meta = read_git_meta(root).unwrap();
        assert_eq!(meta.branch.as_deref(), Some("main"));
        assert!(meta.head_sha.is_none()); // no commits yet
        assert!(!meta.is_detached);
    }

    #[test]
    fn read_git_meta_with_loose_ref() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        let sha = "a".repeat(40);
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(root.join(".git/refs/heads")).unwrap();
        fs::write(root.join(".git/refs/heads/main"), format!("{sha}\n")).unwrap();

        let meta = read_git_meta(root).unwrap();
        assert_eq!(meta.head_sha.as_deref(), Some(sha.as_str()));
        assert_eq!(meta.branch.as_deref(), Some("main"));
        assert!(!meta.is_detached);
    }

    #[test]
    fn read_git_meta_packed_refs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        let sha = "b".repeat(40);
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        // No loose ref — only packed-refs.
        fs::write(
            root.join(".git/packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted\n{sha} refs/heads/main\n"),
        )
        .unwrap();

        let meta = read_git_meta(root).unwrap();
        assert_eq!(meta.head_sha.as_deref(), Some(sha.as_str()));
    }

    #[test]
    fn read_git_meta_detached_head() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        make_bare_repo(root);
        let sha = "c".repeat(40);
        fs::write(root.join(".git/HEAD"), format!("{sha}\n")).unwrap();

        let meta = read_git_meta(root).unwrap();
        assert!(meta.is_detached);
        assert_eq!(meta.head_sha.as_deref(), Some(sha.as_str()));
        assert!(meta.branch.is_none());
    }

    #[test]
    fn discover_nested_git_roots_finds_all_child_repositories() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let repo_a = root.join("group-a/repo-a");
        let repo_b = root.join("group-b/repo-b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();
        make_bare_repo(&repo_a);
        make_bare_repo(&repo_b);
        fs::write(root.join("readme.txt"), "container only").unwrap();

        let roots =
            discover_nested_git_roots(root, &attic_core::CancellationToken::default()).unwrap();

        assert_eq!(
            roots,
            vec![
                repo_a.canonicalize().unwrap(),
                repo_b.canonicalize().unwrap()
            ],
        );
    }

    #[test]
    fn looks_like_sha_rejects_short() {
        assert!(!looks_like_sha("abc123"));
        assert!(!looks_like_sha(""));
    }

    #[test]
    fn looks_like_sha_accepts_40_hex() {
        assert!(looks_like_sha(&"f".repeat(40)));
    }

    #[test]
    fn looks_like_sha_rejects_non_hex() {
        let s = format!("{}z{}", "a".repeat(19), "a".repeat(20));
        assert!(!looks_like_sha(&s));
    }
}
