//! Phase 7 — final platform-appropriate configuration/data/cache/temp policy.
//!
//! ## Policy (normative)
//!
//! Attic distinguishes **user-global** state from **per-workspace** state:
//!
//! * **User-global** (lives under the OS application-data directory for the
//!   current user): the Attic SQLite database (`attic.db`), the optional
//!   semantic layer database (`semantic.db`), crash-recovery backups
//!   (`backups/`), logs written by operators, and scratch/temp files created
//!   during indexing.  These aggregate state across ALL indexed workspaces, so
//!   they cannot meaningfully live inside one workspace.
//! * **Per-workspace**: nothing is persisted inside a workspace.  Attic reads
//!   workspace files and stores every index artifact in the user-global
//!   database, keyed by repository identity.  A workspace is only ever a
//!   *source* directory; Attic never writes `.attic/` into it.  This keeps
//!   workspaces clean for git and avoids polluting projects that are indexed
//!   read-only.
//!
//! Resolution order for the user-global data root:
//! 1. `ATTIC_DATA_DIR` environment variable (explicit operator override).
//! 2. `ATTIC_DB_PATH`'s parent directory when `ATTIC_DB_PATH` is set
//!    (backwards compatibility with the Phase 1D single-variable override).
//! 3. Platform default:
//!    * Windows: `%LOCALAPPDATA%\attic` (falling back to
//!      `%USERPROFILE%\AppData\Local\attic`)
//!    * macOS: `~/Library/Application Support/attic`
//!    * Linux/BSD: `$XDG_DATA_HOME/attic` (XDG spec) falling back to
//!      `~/.local/share/attic`
//! 4. `./attic-data` in the current directory as a last resort (portable run),
//!    so the server never fails to start merely because no home directory is
//!    configured.
//!
//! Derived locations, all relative to the data root:
//! * `attic.db` — main database
//! * `semantic.db` — semantic layer (only created on explicit `ATTIC_SEMANTIC=1`)
//! * `backups/` — crash-recovery database backups
//! * `tmp/` — process scratch space (created on demand; safe to delete while
//!   Attic is not running)

use std::path::{Path, PathBuf};

/// Resolved Attic runtime locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtticPaths {
    /// User-global data root (see module docs).
    pub data_root: PathBuf,
}

impl AtticPaths {
    /// Resolve the data root and derived paths according to the policy above.
    pub fn resolve() -> Self {
        Self {
            data_root: resolve_data_root(),
        }
    }

    /// Path of the main SQLite database (`attic.db`).
    pub fn db_path(&self) -> PathBuf {
        self.data_root.join("attic.db")
    }

    /// Path of the semantic layer database (`semantic.db`).
    pub fn semantic_db_path(&self) -> PathBuf {
        self.data_root.join("semantic.db")
    }

    /// Directory holding crash-recovery backups.
    pub fn backups_dir(&self) -> PathBuf {
        self.data_root.join("backups")
    }

    /// Directory for process scratch files (created on demand).
    pub fn temp_dir(&self) -> PathBuf {
        self.data_root.join("tmp")
    }

    /// Create the data root and derived directories on disk.
    ///
    /// Returns an error string if directory creation fails; the server treats
    /// this as fail-closed (it cannot serve without its data directory).
    pub fn ensure_dirs(&self) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(&self.data_root)?;
        std::fs::create_dir_all(self.backups_dir())?;
        std::fs::create_dir_all(self.temp_dir())?;
        Ok(())
    }
}

/// Resolve the user-global data root (see module docs for the order).
pub fn resolve_data_root() -> PathBuf {
    // 1. Explicit operator override.
    if let Ok(dir) = std::env::var("ATTIC_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    // 2. Legacy Phase 1D override: derive the root from ATTIC_DB_PATH's parent.
    if let Ok(db) = std::env::var("ATTIC_DB_PATH")
        && let Some(parent) = Path::new(&db).parent()
        && !parent.as_os_str().is_empty()
    {
        return parent.to_path_buf();
    }
    // 3. Platform default.
    platform_data_root().unwrap_or_else(|| PathBuf::from("attic-data"))
}

fn platform_data_root() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        // %LOCALAPPDATA%\attic, falling back to %USERPROFILE%\AppData\Local.
        if let Ok(dir) = std::env::var("LOCALAPPDATA")
            && !dir.trim().is_empty()
        {
            return Some(PathBuf::from(dir).join("attic"));
        }
        if let Ok(profile) = std::env::var("USERPROFILE")
            && !profile.trim().is_empty()
        {
            return Some(
                PathBuf::from(profile)
                    .join("AppData")
                    .join("Local")
                    .join("attic"),
            );
        }
        return None;
    }
    if cfg!(target_os = "macos") {
        if let Ok(home) = std::env::var("HOME")
            && !home.trim().is_empty()
        {
            return Some(
                PathBuf::from(home)
                    .join("Library")
                    .join("Application Support")
                    .join("attic"),
            );
        }
        return None;
    }
    // Linux/BSD: XDG Base Directory spec.
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME")
        && !xdg.trim().is_empty()
    {
        return Some(PathBuf::from(xdg).join("attic"));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        return Some(
            PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("attic"),
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_paths_follow_policy() {
        let paths = AtticPaths {
            data_root: PathBuf::from("/data-root"),
        };
        assert_eq!(paths.db_path(), PathBuf::from("/data-root/attic.db"));
        assert_eq!(
            paths.semantic_db_path(),
            PathBuf::from("/data-root/semantic.db")
        );
        assert_eq!(paths.backups_dir(), PathBuf::from("/data-root/backups"));
        assert_eq!(paths.temp_dir(), PathBuf::from("/data-root/tmp"));
    }

    #[test]
    fn ensure_dirs_creates_layout() {
        let tmp = std::env::temp_dir().join(format!("attic_paths_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let paths = AtticPaths {
            data_root: tmp.join("root"),
        };
        paths.ensure_dirs().expect("ensure_dirs");
        assert!(paths.db_path().parent().unwrap().is_dir());
        assert!(paths.backups_dir().is_dir());
        assert!(paths.temp_dir().is_dir());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
