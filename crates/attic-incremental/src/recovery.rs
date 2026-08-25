//! Crash / power-loss recovery + authoritative reconciliation.
//!
//! Startup sequence (recovery contract §3, Phase 2 scope):
//! 1. interrupted `ops_tasks` RUNNING → PENDING (idempotent);
//! 2. incomplete `ops_indexing_log` RUNNING → ABANDONED;
//! 3. occurrences left PENDING_REFRESH → STALE (rescheduled below);
//! 4. secret scans stuck IN_PROGRESS → PENDING;
//! 5. watcher epoch bumped in `ops_server_state`.
//!
//! After the server is serving (REC-W2), a background **reconciliation** walk
//! compares persisted state against actual disk content and schedules exactly
//! the missing work.  Recovery is idempotent across repeated restarts.

use serde::Serialize;
use tracing::{info, warn};

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
    /// Indexing runs marked ABANDONED.
    pub indexing_runs_abandoned: u64,
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

        conn.execute(
            "UPDATE ops_indexing_log
                SET status = 'ABANDONED', completed_at = ?1
              WHERE status = 'RUNNING'",
            [crate::now_micros()],
        )?;
        let indexing_runs_abandoned = conn.changes();

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
            indexing_runs_abandoned,
            refreshes_rescheduled,
            secret_scans_reset,
            watcher_epoch,
            previous_shutdown_clean: previous_clean,
        })
    })?;

    info!(
        tasks_reset = report.tasks_reset,
        abandoned_runs = report.indexing_runs_abandoned,
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

/// Authoritative bounded rescan of one repository root.
///
/// Runs a full Phase 1B discovery walk (the only trusted source when events
/// may have been lost), diffs it against persisted occurrence snapshots, and
/// returns the verified change set WITHOUT mutating anything — callers apply
/// invalidation and schedule recomputation separately.
///
/// Idempotent: running twice over unchanged trees yields empty change sets.
pub fn reconcile_repository(
    pool: &DbPool,
    _writer: &WriterQueueHandle,
    root: &std::path::Path,
    policy: &DiscoveryPolicy,
) -> Result<ReconcileReport, IncrementalError> {
    let discovery = attic_discovery::discover(root, policy)?;
    let repo_root_str = root.to_string_lossy().to_string();
    let Some(repo_id) =
        pool.with_reader(|c| attic_storage::lookup_repository_by_root_path(c, &repo_root_str))?
    else {
        return Err(IncrementalError::NotBootstrapped(repo_root_str));
    };

    // Persisted latest snapshot per path (any freshness).
    let mut db_paths: std::collections::BTreeMap<String, OccurrenceRow> =
        std::collections::BTreeMap::new();
    let rows: Vec<(String, String, String, String)> = pool.with_reader(|conn| {
        let mut stmt = conn.prepare(
            "WITH latest AS (
                 SELECT fo.path AS p, MAX(fo.rowid) AS m
                   FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1
                  GROUP BY fo.path
             )
             SELECT fo.path, fo.content_hash, fo.freshness_state, fo.existence_state
               FROM core_file_occurrences fo
               JOIN latest ON fo.path = latest.p AND fo.rowid = latest.m",
        )?;
        let mapped = stmt.query_map([repo_id.to_string_repr()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r?);
        }
        Ok(out)
    })?;
    for (path, hash, fresh, exist) in rows {
        db_paths.insert(
            path,
            OccurrenceRow {
                content_hash: hash,
                freshness: fresh,
                existence: exist,
            },
        );
    }

    // Disk truth from the walk — three-state reads.  Only verified
    // `NotFound`-class absence may later become a deletion; unreadable or
    // hash-failing paths are recorded as UNCERTAIN and degrade to UNKNOWN.
    let mut disk: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let mut uncertain: Vec<String> = Vec::new();
    for entry in &discovery.entries {
        if entry.abs_path.is_dir() {
            uncertain.push(entry.repo_relative.clone());
            warn!(path = %entry.repo_relative, "reconcile: path became a directory");
            continue;
        }
        match changeset::hash_file(&entry.abs_path) {
            Ok(h) => {
                disk.insert(entry.repo_relative.clone(), h);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Verified absence: omit from disk map → delete candidate in
                // the DB→disk diff below.
            }
            Err(e) => {
                uncertain.push(entry.repo_relative.clone());
                warn!(path = %entry.repo_relative, error = %e, "reconcile: read failed → uncertain");
            }
        }
    }

    let mut report = ReconcileReport {
        repository_id: repo_id.to_string_repr(),
        ..Default::default()
    };
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
        if !disk.contains_key(path) && !uncertain.contains(path) && row.existence != "deleted" {
            cs.deletes.push(path.clone());
            report.newly_excluded += 1;
        }
    }

    cs.uncertain = uncertain;
    report.change_set = cs;
    Ok(report)
}

#[derive(Debug, Default, Clone)]
struct OccurrenceRow {
    content_hash: String,
    freshness: String,
    existence: String,
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
}

/// Record a clean-shutdown marker through the coordinated writer queue.
///
/// The next startup uses this to distinguish a clean stop from a crash.
pub fn record_clean_shutdown_marker(writer: &WriterQueueHandle) -> Result<(), IncrementalError> {
    run_on_writer(writer, |conn| {
        attic_storage::record_clean_shutdown(conn, crate::now_micros())
    })
}

/// Enqueue a RECONCILIATION task (deduped; workspace scope).
pub fn schedule_reconciliation(writer: &WriterQueueHandle) -> Result<bool, IncrementalError> {
    const RECONCILE_PRIORITY: i64 = 40;
    let id = uuid::Uuid::new_v4().to_string();
    let outcome = run_on_writer(writer, move |conn| {
        enqueue_task(
            conn,
            &id,
            None,
            TASK_RECONCILIATION,
            RECONCILE_PRIORITY,
            "{}",
            crate::now_micros(),
        )
    })?;
    Ok(matches!(outcome, attic_storage::EnqueueOutcome::Created))
}
