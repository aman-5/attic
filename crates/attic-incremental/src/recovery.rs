//! Crash / power-loss recovery + authoritative reconciliation.
//!
//! Startup sequence (recovery contract §3, Phase 2 scope):
//! 1. interrupted `ops_tasks` RUNNING → PENDING (idempotent);
//! 2. occurrences left PENDING_REFRESH → STALE (rescheduled below);
//! 3. secret scans stuck IN_PROGRESS → PENDING;
//! 4. watcher epoch bumped in `ops_server_state`.
//!
//! After the server is serving (REC-W2), a background **reconciliation** walk
//! compares persisted state against actual disk content and schedules exactly
//! the missing work.  Recovery is idempotent across repeated restarts.

use serde::Serialize;
use tracing::{info, warn};

use rusqlite::OptionalExtension;

use attic_discovery::DiscoveryPolicy;
use attic_storage::{
    DbPool, TASK_RECONCILIATION, WriterQueueHandle, enqueue_task, get_server_state, record_startup,
    recover_interrupted_tasks,
};

use crate::{IncrementalError, VerifiedChangeSet, changeset, run_on_writer};

/// What startup recovery found and did.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct RecoveryReport {
    /// RUNNING tasks reset to PENDING.
    pub tasks_reset: u64,
    /// Occurrences moved PENDING_REFRESH → STALE.
    pub refreshes_rescheduled: u64,
    /// Secret scans IN_PROGRESS → PENDING.
    pub secret_scans_reset: u64,
    /// New watcher epoch after bump.
    pub watcher_epoch: i64,
    /// Whether the previous shutdown recorded a clean stop.
    pub previous_shutdown_clean: bool,
}

/// Execute the startup recovery procedure (idempotent).
pub fn run_startup_recovery(
    _pool: &DbPool,
    writer: &WriterQueueHandle,
) -> Result<RecoveryReport, IncrementalError> {
    let report = run_on_writer(writer, |conn| {
        let previous_clean = get_server_state(conn)?
            .and_then(|s| s.last_shutdown_at)
            .is_some();

        let tasks_reset = recover_interrupted_tasks(conn)?;

        // PENDING_REFRESH at startup means recomputation was scheduled but a
        // crash hit before completion — back to STALE so it is rescheduled.
        conn.execute(
            "UPDATE core_file_occurrences
                SET freshness_state = 'STALE'
              WHERE freshness_state = 'PENDING_REFRESH'",
            [],
        )?;
        let refreshes_rescheduled = conn.changes();

        conn.execute(
            "UPDATE core_file_occurrences
                SET secret_scan_state = 'PENDING'
              WHERE secret_scan_state = 'IN_PROGRESS'",
            [],
        )?;
        let secret_scans_reset = conn.changes();

        let watcher_epoch = record_startup(
            conn,
            attic_core::constants::CURRENT_SCHEMA_VERSION,
            env!("CARGO_PKG_VERSION"),
            "phase2",
            crate::now_micros(),
        )?;

        Ok(RecoveryReport {
            tasks_reset,
            refreshes_rescheduled,
            secret_scans_reset,
            watcher_epoch,
            previous_shutdown_clean: previous_clean,
        })
    })?;

    info!(
        tasks_reset = report.tasks_reset,
        rescheduled = report.refreshes_rescheduled,
        epoch = report.watcher_epoch,
        "startup recovery complete"
    );
    Ok(report)
}

/// Count occurrences that are not CURRENT (offline-refresh workload).
#[derive(Debug, Default, Clone, Serialize)]
pub struct OfflineRefresh {
    /// Paths needing re-index (STALE/UNKNOWN/PENDING_REFRESH, present).
    pub upsert_paths: Vec<String>,
    /// Repository id those paths belong to.
    pub repository_id: String,
}

/// Read non-CURRENT occurrences grouped per repository.
pub fn plan_offline_refresh(pool: &DbPool) -> Result<Vec<OfflineRefresh>, IncrementalError> {
    let rows: Vec<(String, String)> = pool.with_reader(|conn| {
        let mut stmt = conn.prepare(
            "SELECT fi.repository_id, fo.path
               FROM core_file_occurrences fo
               JOIN core_file_identities fi ON fo.file_identity_id = fi.id
              WHERE fo.freshness_state IN ('STALE', 'UNKNOWN', 'PENDING_REFRESH')
              ORDER BY fi.repository_id, fo.path",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    })?;

    let mut grouped: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for (repo, path) in rows {
        grouped.entry(repo).or_default().push(path);
    }
    Ok(grouped
        .into_iter()
        .map(|(repository_id, upsert_paths)| OfflineRefresh {
            repository_id,
            upsert_paths,
        })
        .collect())
}

/// Racy-file window: a file whose last-write or change time is within this
/// distance of (or after) the moment its metadata was captured is always
/// re-hashed, because a write in that window may not have moved the
/// timestamp past the capture (Git's racy-git rule; 2 s also covers FAT's
/// timestamp granularity).
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

/// Options for [`reconcile_repository_with`].
#[derive(Debug, Clone, Default)]
pub struct ReconcileOptions {
    /// Skip the metadata fast path and hash every file (configured per
    /// membership for network / cloud-sync folders whose timestamps cannot
    /// be trusted).
    pub always_hash: bool,
}

/// Authoritative bounded rescan of one repository root.
///
/// Walks the root (the only trusted source when events may have been lost),
/// diffs it against persisted occurrence snapshots, and returns the verified
/// change set WITHOUT mutating anything — callers apply invalidation and
/// schedule recomputation separately.
///
/// Idempotent: running twice over unchanged trees yields empty change sets.
pub fn reconcile_repository(
    pool: &DbPool,
    _writer: &WriterQueueHandle,
    root: &std::path::Path,
    policy: &DiscoveryPolicy,
) -> Result<ReconcileReport, IncrementalError> {
    reconcile_repository_with(pool, root, policy, &ReconcileOptions::default())
}

/// [`reconcile_repository`] with explicit options.
///
/// Safety rules (R2 — uncertain is never deleted):
/// - a missing root, or an empty walk over a repository that has indexed
///   files, is `SOURCE_UNAVAILABLE` (an unmounted drive must not wipe the
///   index);
/// - a directory the walker could not read makes every indexed path under it
///   uncertain; a walk error with no known location suppresses all
///   deletions for this run;
/// - an unknown or changed root identity disables the metadata fast path;
///   a changed identity suppresses deletions unless the walk was clean.
///
/// Legacy root identities lack creation time and are treated as changed
/// until a successful index publication records the new identity. An empty
/// root with a legacy or unavailable identity requires explicit verification
/// and reindexing rather than inferring deletions.
///
/// Fast path: a file is unchanged WITHOUT reading it only when size,
/// last-write time, change time and file identity all equal the stored
/// values and both timestamps are older than `captured_at - RACY_WINDOW_NS`.
pub fn reconcile_repository_with(
    pool: &DbPool,
    root: &std::path::Path,
    policy: &DiscoveryPolicy,
    opts: &ReconcileOptions,
) -> Result<ReconcileReport, IncrementalError> {
    let repo_root_str = root.to_string_lossy().to_string();
    if !root.is_dir() {
        return Err(IncrementalError::SourceUnavailable(format!(
            "repository root {repo_root_str} is missing or not a directory"
        )));
    }
    let Some(repo_id) =
        pool.with_reader(|c| attic_storage::lookup_repository_by_root_path(c, &repo_root_str))?
    else {
        return Err(IncrementalError::NotBootstrapped(repo_root_str));
    };

    let stored_identity: Option<String> = pool.with_reader(|c| {
        Ok(c.query_row(
            "SELECT root_identity FROM core_repositories WHERE id = ?1",
            [repo_id.to_string_repr()],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
    })?;
    let observed_identity = attic_discovery::root_identity(root);
    let identity_changed = matches!(
        (&stored_identity, &observed_identity),
        (Some(stored), Some(observed)) if stored != observed
    );
    let same_root = stored_identity.is_some() && stored_identity == observed_identity;
    let always_hash = opts.always_hash || !same_root;

    let canonical = root.canonicalize().map_err(|e| {
        IncrementalError::SourceUnavailable(format!("cannot resolve {repo_root_str}: {e}"))
    })?;
    let walk = attic_discovery::walk::walk(&canonical, policy)?;

    // Directories the walker could not read (R2): everything indexed under
    // them is uncertain; an error with no location poisons the whole run.
    let mut uncertain_prefixes: Vec<String> = Vec::new();
    let mut unknown_walk_error = false;
    for d in &walk.diagnostics {
        if d.kind != attic_discovery::DiagnosticKind::IoError {
            continue;
        }
        match d.path.strip_prefix(&canonical) {
            Ok(rel) if !rel.as_os_str().is_empty() => {
                let mut rel = rel.to_string_lossy().replace('\\', "/");
                if !rel.ends_with('/') {
                    rel.push('/');
                }
                uncertain_prefixes.push(rel);
            }
            _ => unknown_walk_error = true,
        }
    }
    let walk_had_errors = unknown_walk_error || !uncertain_prefixes.is_empty();

    // Persisted latest snapshot per path (any freshness).
    let mut db_paths: std::collections::BTreeMap<String, OccurrenceRow> =
        std::collections::BTreeMap::new();
    let rows: Vec<(String, OccurrenceRow)> = pool.with_reader(|conn| {
        let mut stmt = conn.prepare(
            "WITH latest AS (
                 SELECT fo.path AS p, MAX(fo.occurrence_seq) AS m
                   FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1
                  GROUP BY fo.path
             )
             SELECT fo.path, fo.content_hash, fo.freshness_state, fo.existence_state,
                    fo.size_bytes, fo.mtime_ns, fo.change_time_ns, fo.file_id,
                    fo.captured_at_ns
               FROM core_file_occurrences fo
               JOIN latest ON fo.path = latest.p AND fo.occurrence_seq = latest.m",
        )?;
        let mapped = stmt.query_map([repo_id.to_string_repr()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                OccurrenceRow {
                    content_hash: r.get(1)?,
                    freshness: r.get(2)?,
                    existence: r.get(3)?,
                    size_bytes: r.get(4)?,
                    mtime_ns: r.get(5)?,
                    change_time_ns: r.get(6)?,
                    file_id: r.get(7)?,
                    captured_at_ns: r.get(8)?,
                },
            ))
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok(out)
    })?;
    db_paths.extend(rows);

    // An empty walk over a repository with indexed files is trusted as real
    // deletions only when the best available root identity matches the last
    // publish; an unknown or changed identity must not wipe the index (R2).
    let indexed_present = db_paths.values().any(|r| r.existence != "deleted");
    if walk.entries.is_empty() && indexed_present && !same_root {
        return Err(IncrementalError::SourceUnavailable(format!(
            "repository root {repo_root_str} is empty and its identity does not match the indexed root (unmounted or replaced); refusing to infer deletions"
        )));
    }

    let mut report = ReconcileReport {
        repository_id: repo_id.to_string_repr(),
        observed_root_identity: observed_identity,
        root_identity_changed: identity_changed,
        ..Default::default()
    };

    // Disk truth from the walk — three-state reads.  Only verified
    // `NotFound`-class absence may later become a deletion; unreadable or
    // hash-failing paths are recorded as UNCERTAIN and degrade to UNKNOWN.
    let mut disk: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut uncertain: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in &walk.entries {
        if entry.abs_path.is_dir() {
            uncertain.insert(entry.repo_relative.clone());
            warn!(path = %entry.repo_relative, "reconcile: path became a directory");
            continue;
        }
        if !always_hash
            && let Some(row) = db_paths.get(&entry.repo_relative)
            && row.existence != "deleted"
            && row.freshness == "CURRENT"
            && let Ok(meta) = attic_discovery::file_meta(&entry.abs_path)
            && row.metadata_unchanged(&meta)
        {
            disk.insert(entry.repo_relative.clone(), row.content_hash.clone());
            report.files_unchanged_by_metadata += 1;
            continue;
        }
        report.files_hashed += 1;
        match changeset::hash_file(&entry.abs_path) {
            Ok(h) => {
                disk.insert(entry.repo_relative.clone(), h);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Verified absence: omit from disk map → delete candidate in
                // the DB→disk diff below.
            }
            Err(e) => {
                uncertain.insert(entry.repo_relative.clone());
                warn!(path = %entry.repo_relative, error = %e, "reconcile: read failed → uncertain");
            }
        }
    }

    let mut cs = VerifiedChangeSet::default();

    // Disk → DB diff: added/modified.
    for (path, hash) in &disk {
        match db_paths.get(path) {
            None => {
                cs.upserts.push(path.clone());
                report.changed_paths += 1;
            }
            Some(row) if row.content_hash != *hash && row.existence != "deleted" => {
                cs.upserts.push(path.clone());
                report.changed_paths += 1;
            }
            Some(row) if row.existence == "deleted" => {
                // Recreated after deletion tombstone.
                cs.upserts.push(path.clone());
                report.changed_paths += 1;
            }
            Some(row)
                if row.freshness != "CURRENT"
                    && row.existence != "deleted"
                    && row.content_hash == *hash =>
            {
                // Disk verifies the stored hash: trust re-established
                // WITHOUT recomputation (UNKNOWN → CURRENT is a legal
                // verified transition).
                cs.restored.push(path.clone());
            }
            Some(_) => {}
        }
    }

    // DB → disk diff: deleted or newly excluded by policy.
    for (path, row) in &db_paths {
        if row.existence == "deleted" || disk.contains_key(path) || uncertain.contains(path) {
            continue;
        }
        let under_unreadable_dir = uncertain_prefixes.iter().any(|p| path.starts_with(p));
        if under_unreadable_dir || unknown_walk_error || (identity_changed && walk_had_errors) {
            // Absent from the walk, but the walk could not see everything:
            // never a deletion (R2).
            uncertain.insert(path.clone());
            continue;
        }
        cs.deletes.push(path.clone());
        report.newly_excluded += 1;
    }

    cs.uncertain = uncertain.into_iter().collect();
    report.change_set = cs;
    Ok(report)
}

#[derive(Debug, Default, Clone)]
struct OccurrenceRow {
    content_hash: String,
    freshness: String,
    existence: String,
    size_bytes: i64,
    mtime_ns: Option<i64>,
    change_time_ns: Option<i64>,
    file_id: Option<String>,
    captured_at_ns: Option<i64>,
}

impl OccurrenceRow {
    /// Fast-path test (see [`reconcile_repository_with`]). Any unknown
    /// field (older rows, platforms without change time / file id) fails
    /// closed and forces a hash.
    fn metadata_unchanged(&self, meta: &attic_discovery::FileMeta) -> bool {
        let (Some(mtime), Some(ctime), Some(file_id), Some(captured)) = (
            self.mtime_ns,
            self.change_time_ns,
            self.file_id.as_deref(),
            self.captured_at_ns,
        ) else {
            return false;
        };
        let Some(meta_file_id) = meta.file_id.as_deref() else {
            return false;
        };
        u64::try_from(self.size_bytes).ok() == Some(meta.size)
            && mtime == meta.mtime_ns
            && Some(ctime) == meta.change_time_ns
            && file_id == meta_file_id
            && mtime.max(ctime).saturating_add(RACY_WINDOW_NS) < captured
    }
}

/// Result of one reconciliation pass.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Repository the change set belongs to (empty when not bootstrapped).
    pub repository_id: String,
    /// Verified change set (applied by caller).
    pub change_set: VerifiedChangeSet,
    /// Added/modified count.
    pub changed_paths: usize,
    /// Deleted-or-excluded count.
    pub newly_excluded: usize,
    /// Files whose bytes were read and hashed.
    pub files_hashed: usize,
    /// Files proven unchanged by metadata alone (not read).
    pub files_unchanged_by_metadata: usize,
    /// Root identity observed during this pass (record after a successful
    /// publish, or directly when nothing changed).
    pub observed_root_identity: Option<String>,
    /// The root's identity differs from the recorded one (re-clone /
    /// different volume): fast path was disabled for this pass.
    pub root_identity_changed: bool,
}

/// Record a clean-shutdown marker through the coordinated writer queue.
///
/// The next startup uses this to distinguish a clean stop from a crash.
pub fn record_clean_shutdown_marker(writer: &WriterQueueHandle) -> Result<(), IncrementalError> {
    run_on_writer(writer, |conn| {
        attic_storage::record_clean_shutdown(conn, crate::now_micros())
    })
}

/// Enqueue a RECONCILIATION task for one repository.
///
/// Deduplicated per repository, so every configured repository can have one
/// outstanding reconciliation at a time without colliding. Returns `true`
/// when a new task was created (`false` = one was already pending).
pub fn schedule_reconciliation(
    writer: &WriterQueueHandle,
    repository_id: &str,
) -> Result<bool, IncrementalError> {
    const RECONCILE_PRIORITY: i64 = 40;
    let id = uuid::Uuid::new_v4().to_string();
    let repository_id = repository_id.to_owned();
    let outcome = run_on_writer(writer, move |conn| {
        enqueue_task(
            conn,
            &id,
            Some(&repository_id),
            TASK_RECONCILIATION,
            RECONCILE_PRIORITY,
            "{}",
            crate::now_micros(),
        )
    })?;
    Ok(matches!(outcome, attic_storage::EnqueueOutcome::Created))
}
