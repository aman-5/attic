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
//! ├── attic-server(.exe), DirectML.dll — installed binary (MCP configs point here)
//! ├── config/       — config.toml (workspace membership), attic.toml (tunables)
//! ├── data/         — attic.db, semantic.db (+ SQLite -wal/-shm)
//! ├── run/          — attic.lock, attic.ipc (daemon election)
//! ├── models/       — model cache, created lazily when models are downloaded
//! ├── knowledge/    — central project-knowledge notes, created on first start
//! ├── logs/         — file logs, created lazily when file logging is enabled
//! └── backups/      — crash-recovery backups, created lazily on shutdown backup
//! ```
//!
//! The structured layout is used only for a database that
//! [`AtticPaths::resolve`] placed in `<home>/data/` itself (the default, no
//! `ATTIC_DB_PATH`). An explicit `ATTIC_DB_PATH` — even one inside a
//! directory named `data` — keeps the historical FLAT layout, every file
//! beside the database, so existing overrides, scripts and tests keep
//! working unchanged. [`sibling`] maps a file name to its location for the
//! layout in force. A legacy flat `~/.attic` is migrated once on startup
//! (see [`migrate_legacy_layout`]).

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The `data/` directory of the structured home chosen by
/// [`AtticPaths::resolve`] in this process, if any. Only a database directly
/// inside it uses the structured layout; the decision is explicit rather than
/// inferred from a directory name.
static STRUCTURED_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Location of Attic file `name` relative to the database `db_path`, for the
/// layout this process resolved (see [`sibling_in`]).
pub fn sibling(db_path: &Path, name: &str) -> PathBuf {
    let structured = STRUCTURED_DATA_DIR
        .get()
        .is_some_and(|dir| db_path.parent() == Some(dir.as_path()));
    sibling_in(db_path, name, structured)
}

/// Location of Attic file `name` relative to the database `db_path`.
///
/// Structured layout (database in `<home>/data/`): configs go to
/// `<home>/config/`, lock/ipc to `<home>/run/`, `models`/`logs`/`backups` to
/// `<home>/`, databases stay in `data/`. Flat layout: beside the database.
pub fn sibling_in(db_path: &Path, name: &str, structured: bool) -> PathBuf {
    let parent = db_path.parent().unwrap_or(Path::new("."));
    let Some(home) = parent.parent().filter(|_| structured) else {
        return db_path.with_file_name(name);
    };
    match name {
        "attic.toml" | "config.toml" => home.join("config").join(name),
        "attic.lock" | "attic.ipc" => home.join("run").join(name),
        "models" | "logs" | "backups" | "knowledge" => home.join(name),
        _ => parent.join(name),
    }
}

/// A database and the SQLite companions that must travel with it: a `-wal`
/// separated from its database loses committed transactions.
const DB_GROUPS: &[&[&str]] = &[
    &["semantic.db", "semantic.db-wal", "semantic.db-shm"],
    &["attic.db", "attic.db-wal", "attic.db-shm"],
];

/// Plain files moved from a legacy flat home into `config/`.
const CONFIG_FILES: &[&str] = &["config.toml", "attic.toml"];

/// Outcome of [`migrate_legacy_layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutMigration {
    /// Nothing to migrate (fresh install or already structured).
    NotNeeded,
    /// Legacy files were moved into the structured layout.
    Migrated,
    /// A running legacy Attic holds the legacy lock; use the flat layout for
    /// this run and retry next start.
    DeferredLegacyRunning,
    /// Migration failed (`reason`). Every move made by this attempt was
    /// rolled back, so the home is exactly as it was; retried next start.
    Failed(String),
}

/// Legacy items still to move, as `(from, to)` pairs in a safe order: config
/// files, then each database group with its companions BEFORE the database
/// itself, so a crash mid-group never leaves a moved database without its
/// `-wal`. Also resumes an interrupted earlier run: companions whose database
/// already reached `data/` follow it. A legacy database whose destination is
/// already taken is left alone (never merged or overwritten).
fn pending_moves(home: &Path) -> Vec<(PathBuf, PathBuf)> {
    let data = home.join("data");
    let config = home.join("config");
    let mut moves = Vec::new();
    for f in CONFIG_FILES {
        let (from, to) = (home.join(f), config.join(f));
        if from.is_file() && !to.exists() {
            moves.push((from, to));
        }
    }
    for group in DB_GROUPS {
        let (main, companions) = (group[0], &group[1..]);
        let legacy_main = home.join(main).exists();
        let dest_main = data.join(main).exists();
        let follow = if legacy_main { !dest_main } else { dest_main };
        if !follow {
            continue;
        }
        for c in companions {
            let (from, to) = (home.join(c), data.join(c));
            if from.is_file() && !to.exists() {
                moves.push((from, to));
            }
        }
        if legacy_main {
            moves.push((home.join(main), data.join(main)));
        }
    }
    moves
}

/// One-time move of a legacy flat `home` into the structured layout.
///
/// Guarded by the LEGACY `attic.lock`: if an older Attic still holds it, no
/// file is touched (two processes must never write one database). Each
/// attempt is all-or-nothing: on any failure every file it moved is renamed
/// back. A crash-interrupted earlier run is resumed.
pub fn migrate_legacy_layout(home: &Path) -> LayoutMigration {
    if pending_moves(home).is_empty() {
        return LayoutMigration::NotNeeded;
    }
    let lock_path = home.join("attic.lock");
    let lock = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => return LayoutMigration::Failed(format!("open legacy lock: {e}")),
    };
    if lock.try_lock().is_err() {
        return LayoutMigration::DeferredLegacyRunning;
    }
    // Re-plan under the lock: a sibling may have finished the job meanwhile.
    let moves = pending_moves(home);
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::with_capacity(moves.len());
    for mv in &moves {
        let (from, to) = mv;
        let res = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(from, to));
        if let Err(e) = res {
            // All-or-nothing: put back what this attempt moved, newest first.
            let mut rollback_errors = Vec::new();
            for (f, t) in done.iter().rev() {
                if let Err(re) = std::fs::rename(t, f) {
                    rollback_errors.push(format!("{}: {re}", t.display()));
                }
            }
            let mut reason = format!("move {} -> {}: {e}", from.display(), to.display());
            if !rollback_errors.is_empty() {
                reason.push_str(&format!(
                    "; rollback failed for {}",
                    rollback_errors.join(", ")
                ));
            }
            return LayoutMigration::Failed(reason);
        }
        done.push(mv);
    }
    drop(lock);
    // Stale election files of the old layout; the new ones live in run/.
    let _ = std::fs::remove_file(home.join("attic.ipc"));
    let _ = std::fs::remove_file(&lock_path);
    if done.is_empty() {
        LayoutMigration::NotNeeded
    } else {
        LayoutMigration::Migrated
    }
}

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
}

impl AtticPaths {
    /// Resolve Attic's home directory according to the policy described in the
    /// module documentation, create the home directory itself, and return the
    /// populated `AtticPaths`. Optional subdirectories are created lazily by
    /// the subsystems that first write to them.
    ///
    /// Reads `ATTIC_HOME` from the real environment; delegates to
    /// [`resolve_data_root_from`] for the pure resolution logic.
    pub fn resolve() -> Result<Self, PathResolutionError> {
        let attic_home = std::env::var("ATTIC_HOME").ok();
        let db_override = std::env::var("ATTIC_DB_PATH").ok();
        let user_home = home_dir();

        // ATTIC_DB_PATH is the legacy explicit database override documented by
        // the public configuration contract. When ATTIC_HOME is absent, its
        // parent also becomes the Attic home so config and lazily-created
        // subdirectories remain colocated with the explicitly selected database.
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

        let database = match db_override {
            Some(raw) => PathBuf::from(raw),
            None => {
                // Default home: structured layout, migrating a legacy flat
                // home once. A lock held briefly is most likely a sibling
                // launch of THIS version mid-migration (several MCP clients
                // starting together after an upgrade): wait for it rather
                // than falling back to a flat layout and splitting the
                // database. Only a lock held throughout (an older Attic
                // still running) keeps the flat layout for this run.
                let mut migration = migrate_legacy_layout(&home);
                for _ in 0..8 {
                    if migration != LayoutMigration::DeferredLegacyRunning {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(250));
                    migration = migrate_legacy_layout(&home);
                }
                // Flat only when the legacy database is really still there:
                // never open a flat path whose database has already moved
                // (SQLite would silently create an empty one).
                let keep_flat = matches!(
                    migration,
                    LayoutMigration::DeferredLegacyRunning | LayoutMigration::Failed(_)
                ) && home.join("attic.db").exists();
                if keep_flat {
                    eprintln!(
                        "attic: keeping the legacy flat layout in {} for this run ({migration:?})",
                        home.display()
                    );
                    home.join("attic.db")
                } else {
                    if let LayoutMigration::Failed(reason) = &migration {
                        eprintln!(
                            "attic: legacy layout migration incomplete ({reason}); using {}",
                            home.join("data").display()
                        );
                    }
                    for d in ["data", "config", "run"] {
                        std::fs::create_dir_all(home.join(d)).map_err(|e| {
                            PathResolutionError::new(format!(
                                "failed to create {:?}: {e}",
                                home.join(d)
                            ))
                        })?;
                    }
                    let data = home.join("data");
                    let _ = STRUCTURED_DATA_DIR.set(data.clone());
                    data.join("attic.db")
                }
            }
        };

        Ok(Self {
            config_file: sibling(&database, "config.toml"),
            runtime_config: sibling(&database, "attic.toml"),
            semantic_db: sibling(&database, "semantic.db"),
            database,
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

    // ── structured layout ─────────────────────────────────────────────────

    /// Minimal self-cleaning temp dir (attic-core has no dev-dependencies).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "attic-paths-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sibling_maps_structured_and_flat_layouts() {
        let home = PathBuf::from("/h/.attic");
        let db = home.join("data").join("attic.db");
        assert_eq!(
            sibling_in(&db, "attic.toml", true),
            home.join("config").join("attic.toml")
        );
        assert_eq!(
            sibling_in(&db, "config.toml", true),
            home.join("config").join("config.toml")
        );
        assert_eq!(
            sibling_in(&db, "attic.lock", true),
            home.join("run").join("attic.lock")
        );
        assert_eq!(
            sibling_in(&db, "attic.ipc", true),
            home.join("run").join("attic.ipc")
        );
        assert_eq!(sibling_in(&db, "models", true), home.join("models"));
        assert_eq!(sibling_in(&db, "knowledge", true), home.join("knowledge"));
        assert_eq!(sibling_in(&db, "logs", true), home.join("logs"));
        assert_eq!(sibling_in(&db, "backups", true), home.join("backups"));
        assert_eq!(
            sibling_in(&db, "semantic.db", true),
            home.join("data").join("semantic.db")
        );

        // Explicit ATTIC_DB_PATH elsewhere: historical flat layout.
        let flat = PathBuf::from("/tmp/x/e2e.db");
        assert_eq!(
            sibling_in(&flat, "attic.lock", false),
            PathBuf::from("/tmp/x/attic.lock")
        );
        assert_eq!(
            sibling_in(&flat, "models", false),
            PathBuf::from("/tmp/x/models")
        );
    }

    #[test]
    fn legacy_flat_home_is_migrated_once_and_idempotently() {
        let tmp = TmpDir::new();
        let home = tmp.path();
        for f in [
            "attic.db",
            "attic.db-wal",
            "semantic.db",
            "config.toml",
            "attic.toml",
        ] {
            std::fs::write(home.join(f), f).unwrap();
        }
        std::fs::write(home.join("attic.ipc"), "stale").unwrap();

        assert_eq!(migrate_legacy_layout(home), LayoutMigration::Migrated);
        assert_eq!(
            std::fs::read_to_string(home.join("data").join("attic.db")).unwrap(),
            "attic.db"
        );
        assert!(home.join("data").join("attic.db-wal").exists());
        assert!(home.join("data").join("semantic.db").exists());
        assert!(home.join("config").join("config.toml").exists());
        assert!(home.join("config").join("attic.toml").exists());
        assert!(!home.join("attic.db").exists());
        assert!(!home.join("attic.ipc").exists());
        assert!(!home.join("attic.lock").exists());

        assert_eq!(migrate_legacy_layout(home), LayoutMigration::NotNeeded);
    }

    #[test]
    fn fresh_home_needs_no_migration() {
        let tmp = TmpDir::new();
        assert_eq!(
            migrate_legacy_layout(tmp.path()),
            LayoutMigration::NotNeeded
        );
    }

    #[test]
    fn running_legacy_attic_defers_migration() {
        let tmp = TmpDir::new();
        let home = tmp.path();
        std::fs::write(home.join("attic.db"), "db").unwrap();
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(home.join("attic.lock"))
            .unwrap();
        held.try_lock().unwrap();
        assert_eq!(
            migrate_legacy_layout(home),
            LayoutMigration::DeferredLegacyRunning
        );
        assert!(home.join("attic.db").exists(), "nothing moved while held");
        assert!(!home.join("data").exists());
    }

    #[test]
    fn failed_migration_rolls_back_every_move() {
        let tmp = TmpDir::new();
        let home = tmp.path();
        for f in ["attic.db", "attic.db-wal", "config.toml", "attic.toml"] {
            std::fs::write(home.join(f), f).unwrap();
        }
        // `data` is a FILE: the config moves succeed, the database move fails.
        std::fs::write(home.join("data"), "blocker").unwrap();

        assert!(matches!(
            migrate_legacy_layout(home),
            LayoutMigration::Failed(_)
        ));
        for f in ["attic.db", "attic.db-wal", "config.toml", "attic.toml"] {
            assert_eq!(
                std::fs::read_to_string(home.join(f)).unwrap(),
                f,
                "{f} must be back in the flat home"
            );
        }
        assert!(!home.join("config").join("config.toml").exists());
    }

    #[test]
    fn interrupted_migration_is_resumed() {
        let tmp = TmpDir::new();
        let home = tmp.path();
        // A crash after the database moved but before its config/wal did.
        std::fs::create_dir_all(home.join("data")).unwrap();
        std::fs::write(home.join("data").join("attic.db"), "db").unwrap();
        std::fs::write(home.join("attic.db-wal"), "wal").unwrap();
        std::fs::write(home.join("config.toml"), "cfg").unwrap();

        assert_eq!(migrate_legacy_layout(home), LayoutMigration::Migrated);
        assert_eq!(
            std::fs::read_to_string(home.join("data").join("attic.db-wal")).unwrap(),
            "wal"
        );
        assert_eq!(
            std::fs::read_to_string(home.join("config").join("config.toml")).unwrap(),
            "cfg"
        );
        assert_eq!(migrate_legacy_layout(home), LayoutMigration::NotNeeded);
    }

    #[test]
    fn database_is_moved_after_its_wal() {
        let tmp = TmpDir::new();
        let home = tmp.path();
        for f in ["attic.db", "attic.db-wal", "attic.db-shm"] {
            std::fs::write(home.join(f), f).unwrap();
        }
        let moves = pending_moves(home);
        let pos = |n: &str| moves.iter().position(|(f, _)| f.ends_with(n)).unwrap();
        assert!(pos("attic.db-wal") < pos("attic.db"));
        assert!(pos("attic.db-shm") < pos("attic.db"));
    }

    #[test]
    fn explicit_db_path_in_a_data_dir_stays_flat() {
        let db = PathBuf::from("/explicit/data/attic.db");
        assert_eq!(
            sibling(&db, "config.toml"),
            PathBuf::from("/explicit/data/config.toml")
        );
    }
}
