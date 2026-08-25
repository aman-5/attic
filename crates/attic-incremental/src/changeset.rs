//! Verification: turn coalesced hints into a verified ChangeSet.
//!
//! Canonical change detection compares **actual BLAKE3 content** of the real
//! filesystem against the persisted occurrence snapshot — never timestamps
//! alone.  Hints that prove wrong (duplicate events, no-op touches) are
/// dropped here so they can never reach invalidation or scheduling.
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

use attic_core::RepositoryId;
use attic_storage::{DbPool, OccurrenceSnapshot, lookup_occurrence_snapshot};

use crate::coalesce::CoalescedChange;

/// Verified per-file change class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChange {
    /// File exists on disk, no previous CURRENT occurrence.
    Added,
    /// File exists on disk with a different content hash.
    Modified,
    /// File no longer exists on disk but had a PRESENT occurrence.
    Deleted,
}

/// The verified change set handed to invalidation + scheduling.
#[derive(Debug, Clone, Default)]
pub struct VerifiedChangeSet {
    /// Paths added or modified (content exists now).
    pub upserts: Vec<String>,
    /// Paths deleted.
    pub deletes: Vec<String>,
    /// Verified rename pairs (identical content observed on both ends).
    pub renames: Vec<(String, String)>,
    /// `true` when a discovery-policy input changed (`.gitignore`, policy
    /// config); caller must run targeted rediscovery instead of scoped work.
    pub policy_changed: bool,
}

impl VerifiedChangeSet {
    /// All paths referenced anywhere in this set.
    pub fn touched_paths(&self) -> BTreeSet<String> {
        let mut s: BTreeSet<String> = BTreeSet::new();
        s.extend(self.upserts.iter().cloned());
        s.extend(self.deletes.iter().cloned());
        s.extend(
            self.renames
                .iter()
                .flat_map(|(f, t)| [f.clone(), t.clone()]),
        );
        s
    }
}

/// Baseline snapshot provider — abstracted for deterministic tests.
pub trait SnapshotSource {
    /// Latest persisted occurrence snapshot for one repo+path.
    fn snapshot(&self, path: &str) -> Option<OccurrenceSnapshot>;
}

/// Production [`SnapshotSource`] backed by the read pool.
pub struct DbSnapshotSource<'a> {
    pool: &'a DbPool,
    repo_id: RepositoryId,
}

impl<'a> DbSnapshotSource<'a> {
    /// Create a source for one repository.
    pub fn new(pool: &'a DbPool, repo_id: RepositoryId) -> Self {
        Self { pool, repo_id }
    }
}

impl SnapshotSource for DbSnapshotSource<'_> {
    fn snapshot(&self, path: &str) -> Option<OccurrenceSnapshot> {
        self.pool
            .with_reader(|c| lookup_occurrence_snapshot(c, &self.repo_id, path))
            .ok()
            .flatten()
    }
}

/// Stream-hash a file's raw bytes with BLAKE3.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Verify coalesced hints against actual filesystem + persisted state.
///
/// `root` is the repository root on disk; every hint path is joined under it.
pub fn verify(
    root: &Path,
    ops: Vec<CoalescedChange>,
    snapshots: &dyn SnapshotSource,
) -> VerifiedChangeSet {
    let mut cs = VerifiedChangeSet::default();

    // Deduplicate hints per path first (rename pairs handled after).
    enum Hint {
        Touch,
        Gone,
        RenameTo,
        RenameFrom,
    }
    let mut hints: BTreeMap<String, Hint> = BTreeMap::new();

    for op in ops {
        match op {
            CoalescedChange::Upsert(p) => {
                hints.entry(p).or_insert(Hint::Touch);
            }
            CoalescedChange::Remove(p) => {
                hints.insert(p, Hint::Gone);
            }
            CoalescedChange::Rename(from, to) => {
                hints.entry(to.clone()).or_insert(Hint::RenameTo);
                hints.entry(from.clone()).or_insert(Hint::RenameFrom);
            }
        }
    }

    // Per-path verification pass.  Deterministic: BTreeMap order.
    // disk_state: path → Option<(exists, hash)>
    let mut disk_state: BTreeMap<String, Option<String>> = BTreeMap::new();

    for (path, hint) in &hints {
        if is_policy_input(path) {
            cs.policy_changed = true;
        }
        let abs = root.join(path);
        let hash = hash_file(&abs).ok();
        disk_state.insert(path.clone(), hash);
        match hint {
            Hint::Touch => {}
            Hint::Gone | Hint::RenameFrom | Hint::RenameTo => {}
        }
    }

    // Classify each unique path once.
    let mut added_or_modified: BTreeSet<String> = BTreeSet::new();
    let mut deleted: BTreeSet<String> = BTreeSet::new();
    let mut content_by_path: BTreeMap<String, String> = BTreeMap::new();

    for (path, hint) in &hints {
        let disk_hash = disk_state.get(path).cloned().flatten();
        let snap = snapshots.snapshot(path);

        match (&hint, disk_hash) {
            (_, Some(hash)) => {
                content_by_path.insert(path.clone(), hash.clone());
                let unchanged = matches!(&snap, Some(s)
                    if s.content_hash == hash && s.existence_state != "deleted");
                match &hint {
                    Hint::RenameTo | Hint::Touch => {
                        if !unchanged {
                            added_or_modified.insert(path.clone());
                        }
                    }
                    Hint::Gone => {
                        // Remove hint but file exists again (delete+recreate
                        // collapsed): classify by content comparison.
                        if !unchanged {
                            added_or_modified.insert(path.clone());
                        }
                    }
                    Hint::RenameFrom => {
                        deleted.insert(path.clone());
                    }
                }
            }
            (_, None) => match &hint {
                Hint::RenameFrom | Hint::Gone => {
                    if snap.is_some() {
                        deleted.insert(path.clone());
                    }
                    // No snapshot and no file: transient noise; drop.
                }
                Hint::Touch | Hint::RenameTo => {
                    // Existed at hint time, gone at verify time → deletion.
                    if snap.is_some() {
                        deleted.insert(path.clone());
                    }
                }
            },
        }

        if snap.is_none() && content_by_path.contains_key(path) {
            added_or_modified.insert(path.clone()); // brand-new file
        }
    }

    // Rename resolution: deleted(A) + added(B), identical content → rename.
    // The vanished side has no disk hash, so its PREVIOUS content hash comes
    // from the persisted occurrence snapshot.
    let mut consumed_added: BTreeSet<String> = BTreeSet::new();
    for a in deleted.clone() {
        let a_hash = match content_by_path.get(&a).cloned() {
            Some(h) => Some(h),
            None => snapshots.snapshot(&a).map(|s| s.content_hash),
        };
        let Some(a_hash) = a_hash else {
            continue;
        };
        let candidates: Vec<&String> = added_or_modified
            .iter()
            .filter(|b| *b != &a && content_by_path.get(*b).is_some_and(|h| *h == a_hash))
            .collect();
        if let Some(b) = candidates.first() {
            let b = (*b).clone();
            deleted.remove(&a);
            consumed_added.insert(b.clone());
            // B still needs indexing (its occurrence lives under the new
            // path); record the pairing for identity links.
            cs.renames.push((a, b));
        }
    }

    cs.upserts.extend(added_or_modified.iter().cloned());
    cs.deletes.extend(deleted.iter().cloned());
    cs.upserts.sort();
    cs.deletes.sort();
    cs
}

/// Paths whose modification changes discovery policy semantics.
pub fn is_policy_input(rel_path: &str) -> bool {
    rel_path == ".gitignore"
        || rel_path.ends_with("/.gitignore")
        || rel_path == ".attic-policy.json"
        || rel_path.ends_with("/.attic-policy.json")
}
