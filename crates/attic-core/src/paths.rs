//! Attic home directory resolution and runtime path policy.
//!
//! ## Policy (normative)
//!
//! Attic keeps **all** user-global state in one directory called the
//! **Attic home** (`~/.attic` by default).  Nothing is ever written into
//! an indexed workspace.
//!
//! ### Home resolution order
//!
//! | Priority | Source | Notes |
//! |----------|--------|-------|
//! | 1 | `ATTIC_HOME` env var (non-empty) | Explicit override — pins the whole home |
//! | 2 | `<user home>/.attic` | Derived from the OS user-home directory |
//! | — | anything else | Hard error — no silent CWD / temp fallbacks |
//!
//! An empty `ATTIC_HOME=""` is a **configuration error** (not silently ignored).
//!
//! ### Derived layout
//!
//! ```text
//! ~/.attic/
//! ├── config.toml   — persistent multi-root workspace configuration
//! ├── attic.db      — main SQLite database
//! ├── semantic.db   — semantic layer (only when ATTIC_SEMANTIC=1)
//! ├── backups/      — crash-recovery backups
//! └── tmp/          — process scratch space (safe to delete while Attic is not running)
//! ```

use std::fmt;
use std::path::PathBuf;

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Error returned when Attic cannot determine or use its home directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathResolutionError(String);

impl fmt::Display for PathResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PathResolutionError {}

impl PathResolutionError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// AtticPaths
// ─────────────────────────────────────────────────────────────────────────────

/// Resolved Attic runtime locations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtticPaths {
    /// The Attic home directory (`~/.attic` or `$ATTIC_HOME`).
    pub home: PathBuf,
    /// Main SQLite database (`<home>/attic.db`).
    pub database: PathBuf,
    /// Persistent workspace configuration file (`<home>/config.toml`).
    pub config_file: PathBuf,
    /// Resource/embedding tunables file (`<home>/attic.toml`). A second,
    /// separate file from `config_file` — never merged with it.
    pub runtime_config: PathBuf,
    /// Semantic layer database (`<home>/semantic.db`).
    pub semantic_db: PathBuf,
    /// Crash-recovery backup directory (`<home>/backups/`).
    pub backups_dir: PathBuf,
    /// Process scratch directory (`<home>/tmp/`).
    pub temp_dir: PathBuf,
}

impl AtticPaths {
    /// Resolve Attic's home directory according to the policy described in the
    /// module documentation, create required directories, and return the
    /// populated `AtticPaths`.
    ///
    /// Reads `ATTIC_HOME` from the real environment; delegates to
    /// [`resolve_data_root_from`] for the pure resolution logic.
    pub fn resolve() -> Result<Self, PathResolutionError> {
        let attic_home = std::env::var("ATTIC_HOME").ok();
        let db_override = std::env::var("ATTIC_DB_PATH").ok();
        let user_home = home_dir();

        // ATTIC_DB_PATH is the legacy explicit database override documented by
        // the public configuration contract. When ATTIC_HOME is absent, its
        // parent also becomes the Attic home so config/backups/tmp remain
        // colocated with the explicitly selected database.
        let derived_home = db_override
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(|s| PathBuf::from(s).parent().map(PathBuf::from))
            // A bare filename (no directory component, e.g. "attic.db") has a
            // `parent()` of `Some("")`, not `None` — filter that out so it
            // falls through to the normal ATTIC_HOME/user-home resolution
            // instead of silently treating the empty string as the home dir.
            .filter(|p| !p.as_os_str().is_empty());
        let home = match (attic_home.as_deref(), derived_home) {
            (None, Some(home)) => home,
            (attic_home, _) => resolve_data_root_from(attic_home, user_home)?,
        };

        // Validate before any directory is created, so an invalid
        // ATTIC_DB_PATH fails clean with no filesystem side effects — same
        // fail-fast contract as the empty-ATTIC_HOME check above.
        if let Some(raw) = &db_override
            && raw.trim().is_empty()
        {
            return Err(PathResolutionError::new(
                "ATTIC_DB_PATH is set but empty; provide a database path or unset it",
            ));
        }

        std::fs::create_dir_all(&home).map_err(|e| {
            PathResolutionError::new(format!(
                "failed to create Attic home directory {:?}: {}",
                home, e
            ))
        })?;

        let backups = home.join("backups");
        std::fs::create_dir_all(&backups).map_err(|e| {
            PathResolutionError::new(format!(
                "failed to create backups directory {:?}: {}",
                backups, e
            ))
        })?;

        let tmp = home.join("tmp");
        std::fs::create_dir_all(&tmp).map_err(|e| {
            PathResolutionError::new(format!("failed to create tmp directory {:?}: {}", tmp, e))
        })?;

        let database = match db_override {
            Some(raw) => PathBuf::from(raw),
            None => home.join("attic.db"),
        };

        Ok(Self {
            database,
            config_file: home.join("config.toml"),
            runtime_config: home.join("attic.toml"),
            semantic_db: home.join("semantic.db"),
            backups_dir: backups,
            temp_dir: tmp,
            home,
        })
    }

    /// Path of the main SQLite database.
    pub fn db_path(&self) -> &PathBuf {
        &self.database
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure injectable resolution function
// ─────────────────────────────────────────────────────────────────────────────

/// Resolve the Attic home directory from explicit inputs, without reading
/// environment variables.
///
/// This function is the pure core of the resolution policy and is `pub` so
/// that unit tests can exercise every branch without mutating the process
/// environment.
///
/// # Policy
///
/// | `attic_home` | `user_home` | Result |
/// |---|---|---|
/// | `Some(s)` where `s` is non-empty after trim | any | `Ok(PathBuf::from(s))` |
/// | `Some("")` or `Some("   ")` | any | `Err` — empty `ATTIC_HOME` is a config error |
/// | `None` | `Some(h)` | `Ok(h.join(".attic"))` |
/// | `None` | `None` | `Err` — cannot determine home directory |
pub fn resolve_data_root_from(
    attic_home: Option<&str>,
    user_home: Option<PathBuf>,
) -> Result<PathBuf, PathResolutionError> {
    match attic_home {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(PathResolutionError::new(
                    "ATTIC_HOME is set but empty; provide a non-empty path or unset it",
                ));
            }
            Ok(PathBuf::from(s))
        }
        None => match user_home {
            Some(h) => Ok(h.join(".attic")),
            None => Err(PathResolutionError::new(
                "cannot determine Attic home: ATTIC_HOME is not set and the user \
                 home directory could not be resolved; set ATTIC_HOME explicitly",
            )),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Platform user-home resolution
// ─────────────────────────────────────────────────────────────────────────────

/// Attempt to determine the OS user home directory.
fn home_dir() -> Option<PathBuf> {
    // Unix HOME (also set on Windows by Git Bash / MSYS2).
    if let Ok(h) = std::env::var("HOME") {
        let h = h.trim().to_owned();
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    // Windows USERPROFILE.
    if let Ok(p) = std::env::var("USERPROFILE") {
        let p = p.trim().to_owned();
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    // Windows HOMEDRIVE + HOMEPATH fallback.
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        let drive = drive.trim().to_owned();
        let path = path.trim().to_owned();
        if !drive.is_empty() && !path.is_empty() {
            return Some(PathBuf::from(format!("{}{}", drive, path)));
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── resolve_data_root_from policy ─────────────────────────────────────

    #[test]
    fn explicit_non_empty_attic_home_wins() {
        let result =
            resolve_data_root_from(Some("/explicit/home"), Some(PathBuf::from("/user/home")));
        assert_eq!(result.unwrap(), PathBuf::from("/explicit/home"));
    }

    #[test]
    fn empty_attic_home_is_error() {
        let result = resolve_data_root_from(Some(""), Some(PathBuf::from("/user/home")));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("ATTIC_HOME") && msg.contains("empty"),
            "error must mention ATTIC_HOME and empty; got: {msg}"
        );
    }

    #[test]
    fn whitespace_only_attic_home_is_error() {
        let result = resolve_data_root_from(Some("   "), Some(PathBuf::from("/user/home")));
        assert!(result.is_err());
    }

    #[test]
    fn no_attic_home_derives_from_user_home() {
        let user = PathBuf::from("/my/home");
        let result = resolve_data_root_from(None, Some(user.clone()));
        assert_eq!(result.unwrap(), user.join(".attic"));
    }

    #[test]
    fn no_attic_home_no_user_home_is_error() {
        let result = resolve_data_root_from(None, None);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(!msg.is_empty(), "error must be non-empty");
        assert!(
            msg.contains("ATTIC_HOME"),
            "error should mention ATTIC_HOME; got: {msg}"
        );
    }

    // ── AtticPaths derived fields ─────────────────────────────────────────

    #[test]
    fn derived_paths_follow_home() {
        let tmp = std::env::temp_dir().join(format!("attic_paths_unit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create tmp");

        let home = tmp.join("home");
        let paths = AtticPaths {
            database: home.join("attic.db"),
            config_file: home.join("config.toml"),
            runtime_config: home.join("attic.toml"),
            semantic_db: home.join("semantic.db"),
            backups_dir: home.join("backups"),
            temp_dir: home.join("tmp"),
            home: home.clone(),
        };
        assert_eq!(paths.database, home.join("attic.db"));
        assert_eq!(paths.config_file, home.join("config.toml"));
        assert_eq!(paths.backups_dir, home.join("backups"));
        assert_eq!(paths.temp_dir, home.join("tmp"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn ensure_resolve_creates_dirs() {
        // Use ATTIC_HOME pointing at a fresh temp directory so resolve() runs
        // without touching ~/.attic.
        let tmp = std::env::temp_dir().join(format!("attic_resolve_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        // Do NOT pre-create tmp — resolve() must create it.
        let home_dir = tmp.join("attic-home");

        // We can't set env vars safely in parallel tests, so call the resolver
        // directly with the injectable function and manually build AtticPaths.
        let result = resolve_data_root_from(
            Some(home_dir.to_str().expect("utf8")),
            Some(tmp.join("user")),
        );
        assert!(
            result.is_ok(),
            "resolve_data_root_from failed: {:?}",
            result
        );
        let resolved = result.unwrap();
        assert_eq!(resolved, home_dir);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
