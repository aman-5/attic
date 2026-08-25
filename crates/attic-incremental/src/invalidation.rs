//! Invalidation engine — the cheap, synchronous half of the pipeline.
//!
//! Marks affected occurrences STALE/INVALID and propagates through the
//! artifact dependency DAG inside ONE coordinated writer transaction.
//! Recomputation is NOT triggered here (invalidation ≠ recomputation); the
//! scheduler picks work up separately from `ops_tasks`.

use std::sync::{Arc, Mutex};

use attic_core::{FreshnessState, InvalidationCause};
use attic_storage::{WriterQueueHandle, invalidate_for_occurrences, lookup_occurrence_snapshot};

use crate::changeset::VerifiedChangeSet;

/// Counts from one applied change set.
#[derive(Debug, Default, Clone, Copy)]
pub struct AppliedInvalidation {
    /// Occurrences marked (sum across paths).
    pub occurrences_marked: u64,
    /// Derived artifacts invalidated transitively (sum).
    pub derived_invalidated: u64,
}

/// Apply invalidation for a verified change set of one repository.
///
/// Mapping per contract:
/// - deleted path  → occurrence INVALID (+ all dependents INVALID)
/// - changed path  → occurrence STALE   (+ all dependents INVALID)
///
/// Paths without a PRESENT occurrence are new files — nothing to invalidate.
pub fn apply_invalidation(
    writer: &WriterQueueHandle,
    repo_id: &str,
    cs: &VerifiedChangeSet,
    cause: InvalidationCause,
    now_us: i64,
) -> Result<AppliedInvalidation, crate::IncrementalError> {
    let repo_typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| crate::IncrementalError::NotBootstrapped(repo_id.to_owned()))?;

    // Owned copies for the 'static closure.
    struct Mark {
        path: String,
        target: FreshnessState,
    }
    let mut marks: Vec<Mark> = Vec::new();
    for d in &cs.deletes {
        marks.push(Mark {
            path: d.clone(),
            target: FreshnessState::Invalid,
        });
    }
    for u in &cs.upserts {
        marks.push(Mark {
            path: u.clone(),
            target: FreshnessState::Stale,
        });
    }
    // Rename origins behave like deletions (old path disappears).
    for (f, _) in &cs.renames {
        marks.push(Mark {
            path: f.clone(),
            target: FreshnessState::Invalid,
        });
    }

    if marks.is_empty() {
        return Ok(AppliedInvalidation::default());
    }

    let slot: Arc<Mutex<Option<AppliedInvalidation>>> = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&slot);
    let cause_str = cause;

    writer
        .send(move |conn| {
            let mut acc = AppliedInvalidation::default();
            for m in &marks {
                let Some(snap) = lookup_occurrence_snapshot(conn, &repo_typed, &m.path)? else {
                    continue;
                };
                if snap.existence_state == "deleted" {
                    continue;
                }
                let counts = invalidate_for_occurrences(
                    conn,
                    std::slice::from_ref(&snap.id),
                    m.target,
                    cause_str,
                    now_us,
                )?;
                acc.occurrences_marked += counts.occurrences;
                acc.derived_invalidated += counts.total() - counts.occurrences;
            }
            if let Ok(mut g) = sink.lock() {
                *g = Some(acc);
            }
            Ok(())
        })
        .map_err(crate::IncrementalError::Storage)?;

    match slot.lock() {
        Ok(g) => g.ok_or_else(|| {
            crate::IncrementalError::Storage(attic_storage::StorageError::Worker(
                "invalidation closure completed without recording counts".into(),
            ))
        }),
        Err(_) => Err(crate::IncrementalError::Storage(
            attic_storage::StorageError::MutexPoisoned("invalidation slot poisoned".into()),
        )),
    }
}
