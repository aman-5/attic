//! The freshness gate (`ensure_current`): the single check every
//! index-reading answer must pass first.
//!
//! One call:
//! 1. takes the repository's lock (shared with the scheduler, so at most one
//!    check / index run touches a repository at a time — R6);
//! 2. records the store version, runs the authoritative reconcile (metadata
//!    fast path + hashing of anything that may have changed);
//! 3. re-indexes exactly the changed files through the existing scoped
//!    pipeline, fenced on the recorded store version, in chunks;
//! 4. retries a bounded number of times when files keep changing or another
//!    writer moved the store, then fails with `SOURCE_CHANGING`;
//! 5. returns the resulting store version plus a source label.
//!
//! Watchers, the scheduler and startup recovery may do the same work
//! earlier, but only a [`GateOutcome`] tells an answer which data it may
//! read (R5).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::debug;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, ScopedChanges};
use attic_storage::{DbPool, StoreVersion, WriterQueueHandle, read_store_version};

use crate::IncrementalError;
use crate::recovery::{ReconcileOptions, reconcile_repository_with};

/// Gate tuning (plan §5).
#[derive(Clone)]
pub struct GateConfig {
    /// Back-off before each retry when files kept changing or the store
    /// moved under the check; its length is the retry count.
    pub retry_backoff: Vec<Duration>,
    /// Files published per fenced chunk.
    pub chunk_size: usize,
    /// Hash every file (no metadata fast path).
    pub always_hash: bool,
    /// Scoped-indexing options (same as the scheduler's).
    pub index_options: IndexOptions,
    /// Test hook run after the reconcile and before each publication.
    #[doc(hidden)]
    pub before_publish: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for GateConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateConfig")
            .field("retry_backoff", &self.retry_backoff)
            .field("chunk_size", &self.chunk_size)
            .field("always_hash", &self.always_hash)
            .finish_non_exhaustive()
    }
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            retry_backoff: vec![
                Duration::from_millis(50),
                Duration::from_millis(150),
                Duration::from_millis(450),
            ],
            chunk_size: 500,
            always_hash: false,
            index_options: IndexOptions::default(),
            before_publish: None,
        }
    }
}

/// What the gate verified, for one repository.
#[derive(Debug, Clone, Serialize)]
pub struct GateOutcome {
    /// Repository UUID.
    pub repository_id: String,
    /// Store version the caller must read at (see `read_at_version`).
    pub version: StoreVersion,
    /// Which code this is.
    pub source_label: SourceLabel,
    /// Paths that could not be read this check; their previous index rows
    /// are kept and they are flagged as unverified.
    pub uncertain: Vec<String>,
    /// Files read and hashed by the check.
    pub files_hashed: usize,
    /// Files proven unchanged by metadata alone.
    pub files_unchanged_by_metadata: usize,
    /// Files re-indexed (added/changed/deleted/renamed).
    pub files_reindexed: usize,
    /// `true` when every file was hashed (always_hash or re-clone).
    pub full_hash: bool,
    /// Wall-clock milliseconds spent.
    pub elapsed_ms: u64,
}

/// Identity of the code an answer was built from.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct SourceLabel {
    /// `git` or `folder`.
    pub kind: String,
    /// Current branch (`None` for detached HEAD or plain folders).
    pub branch: Option<String>,
    /// HEAD commit (`None` for plain folders).
    pub commit: Option<String>,
    /// BLAKE3 over every indexed `(path, content hash)`: identifies the
    /// exact checked content, including uncommitted local changes.
    pub content_fingerprint: String,
}

/// Per-repository lock shared by the gate and the scheduler (R6).
pub fn repository_lock(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let key = root.to_string_lossy().to_lowercase();
    let mut map = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    Arc::clone(map.entry(key).or_default())
}

/// Bring one repository's index up to date with its saved files and return
/// the version answers must read at.
pub fn ensure_current(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    root: &Path,
    policy: &DiscoveryPolicy,
    config: &GateConfig,
) -> Result<GateOutcome, IncrementalError> {
    let started = Instant::now();
    let lock = repository_lock(root);
    let _guard = lock.lock().unwrap_or_else(|p| p.into_inner());

    let mut last_reason = String::new();
    for attempt in 0..=config.retry_backoff.len() {
        if attempt > 0 {
            std::thread::sleep(config.retry_backoff[attempt - 1]);
        }
        match check_once(pool, writer, root, policy, config)? {
            Attempt::Done(mut outcome) => {
                outcome.elapsed_ms = started.elapsed().as_millis() as u64;
                return Ok(outcome);
            }
            Attempt::Retry(reason) => {
                debug!(attempt, %reason, "freshness check retry");
                last_reason = reason;
            }
        }
    }
    Err(IncrementalError::SourceChanging(format!(
        "{}: {last_reason}",
        root.display()
    )))
}

enum Attempt {
    Done(GateOutcome),
    Retry(String),
}

fn check_once(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    root: &Path,
    policy: &DiscoveryPolicy,
    config: &GateConfig,
) -> Result<Attempt, IncrementalError> {
    let mut version = current_version(pool)?;
    let report = reconcile_repository_with(
        pool,
        root,
        policy,
        &ReconcileOptions {
            always_hash: config.always_hash,
        },
    )?;
    let repo_id = report.repository_id.clone();
    let mut cs = report.change_set;
    if let Ok(typed) = repo_id.parse::<attic_core::RepositoryId>() {
        crate::service::pair_content_renames(root, pool, &typed, &mut cs)?;
    }

    if !cs.uncertain.is_empty() {
        crate::service::mark_paths_unknown(writer, &repo_id, &cs.uncertain)?;
    }
    if !cs.restored.is_empty() {
        crate::service::apply_restored(writer, &repo_id, &cs.restored)?;
    }
    // Writes above (trust marks) move the store version; they are ours.
    if !cs.uncertain.is_empty() || !cs.restored.is_empty() {
        version = current_version(pool)?;
    }

    let mut uncertain = cs.uncertain.clone();
    let mut reindexed = 0usize;
    let store = IndexingStore {
        readers: pool,
        writer,
    };
    for chunk in chunk_changes(&cs, config.chunk_size.max(1)) {
        if let Some(hook) = &config.before_publish {
            hook();
        }
        let res = attic_indexing::index_changes_fenced(
            &store,
            root,
            policy,
            &config.index_options,
            &chunk,
            Some(version.clone()),
        );
        match res {
            Ok(r) if r.rejected => {
                return Ok(Attempt::Retry(
                    "another writer changed the index during the check".into(),
                ));
            }
            Ok(r) => {
                reindexed +=
                    (chunk.upserts.len() + chunk.deletes.len()).saturating_sub(r.uncertain.len());
                if !r.uncertain.is_empty() {
                    crate::service::mark_paths_unknown(writer, &repo_id, &r.uncertain)?;
                }
                uncertain.extend(r.uncertain);
                version = current_version(pool)?;
            }
            Err(attic_indexing::IndexError::Io { path, source }) => {
                // Unstable read (file changed between hash and analysis).
                return Ok(Attempt::Retry(format!("{path}: {source}")));
            }
            Err(e) => return Err(e.into()),
        }
    }

    // Nothing to publish but the root's identity was refreshed (re-clone
    // with identical content): record it so the fast path re-engages.
    if reindexed == 0
        && report.root_identity_changed
        && let Some(identity) = report.observed_root_identity.clone()
    {
        let rid = repo_id.clone();
        crate::run_on_writer(writer, move |conn| {
            conn.execute(
                "UPDATE core_repositories SET root_identity = ?1 WHERE id = ?2",
                rusqlite::params![identity, rid],
            )?;
            Ok(())
        })?;
        version = current_version(pool)?;
    }

    uncertain.sort();
    uncertain.dedup();
    Ok(Attempt::Done(GateOutcome {
        source_label: source_label(pool, root, &repo_id)?,
        repository_id: repo_id,
        version,
        uncertain,
        files_hashed: report.files_hashed,
        files_unchanged_by_metadata: report.files_unchanged_by_metadata,
        files_reindexed: reindexed,
        full_hash: config.always_hash || report.root_identity_changed,
        elapsed_ms: 0,
    }))
}

fn current_version(pool: &DbPool) -> Result<StoreVersion, IncrementalError> {
    pool.with_reader(read_store_version)?.ok_or_else(|| {
        IncrementalError::Storage(attic_storage::StorageError::Worker(
            "store has no ops_store_version (outdated schema)".into(),
        ))
    })
}

/// Split a verified change set into fenced publication chunks. Deletes,
/// rename hints AND rename targets travel in the first chunk so a rename
/// is never split.
fn chunk_changes(cs: &crate::VerifiedChangeSet, size: usize) -> Vec<ScopedChanges> {
    if !cs.has_verified_work() {
        return Vec::new();
    }
    let mut upserts: Vec<String> = cs.renames.iter().map(|(_, to)| to.clone()).collect();
    upserts.extend(cs.upserts.iter().cloned());
    let mut deletes: Vec<String> = cs.deletes.clone();
    for (from, _) in &cs.renames {
        deletes.push(from.clone());
    }
    let first_size = size.max(cs.renames.len());
    let mut chunks = Vec::new();
    let mut first = true;
    let mut rest = upserts.as_slice();
    loop {
        let take = rest.len().min(if first { first_size } else { size });
        let (head, tail) = rest.split_at(take);
        chunks.push(ScopedChanges {
            upserts: head.to_vec(),
            deletes: if first {
                std::mem::take(&mut deletes)
            } else {
                Vec::new()
            },
            rename_hints: if first {
                cs.renames.clone()
            } else {
                Vec::new()
            },
        });
        first = false;
        rest = tail;
        if rest.is_empty() {
            break;
        }
    }
    chunks
}

fn source_label(
    pool: &DbPool,
    root: &Path,
    repo_id: &str,
) -> Result<SourceLabel, IncrementalError> {
    let typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| IncrementalError::NotBootstrapped(repo_id.to_owned()))?;
    let mut pairs =
        pool.with_reader(|c| attic_storage::current_path_hashes_for_repository(c, &typed))?;
    pairs.sort();
    let mut hasher = blake3::Hasher::new();
    for (p, h) in &pairs {
        hasher.update(p.as_bytes());
        hasher.update(b"\t");
        hasher.update(h.as_bytes());
        hasher.update(b"\n");
    }
    let content_fingerprint = hasher.finalize().to_hex().to_string();
    let git = attic_discovery::git::find_git_root(root, root)
        .and_then(|r| attic_discovery::git::read_git_meta(&r).ok());
    Ok(match git {
        Some(meta) => SourceLabel {
            kind: "git".into(),
            branch: meta.branch,
            commit: meta.head_sha,
            content_fingerprint,
        },
        None => SourceLabel {
            kind: "folder".into(),
            branch: None,
            commit: None,
            content_fingerprint,
        },
    })
}

/// Optional caller assertion about which code must be checked out.
#[derive(Debug, Clone, Default)]
pub struct ExpectedSource {
    /// Required branch name.
    pub branch: Option<String>,
    /// Required HEAD commit (full SHA or unique prefix of at least 7 chars).
    pub commit: Option<String>,
}

/// `BRANCH_MISMATCH` detail when the checked-out code is not what the
/// caller asked for.
pub fn check_expected(label: &SourceLabel, expected: &ExpectedSource) -> Result<(), String> {
    if let Some(want) = &expected.branch
        && label.branch.as_deref() != Some(want.as_str())
    {
        return Err(format!(
            "expected branch '{want}' but '{}' is checked out",
            label.branch.as_deref().unwrap_or(if label.kind == "git" {
                "(detached HEAD)"
            } else {
                "(not a Git repository)"
            })
        ));
    }
    if let Some(want) = &expected.commit {
        let want = want.to_ascii_lowercase();
        let ok = want.len() >= 7
            && label
                .commit
                .as_deref()
                .is_some_and(|c| c.to_ascii_lowercase().starts_with(&want));
        if !ok {
            return Err(format!(
                "expected commit '{want}' but HEAD is '{}'",
                label.commit.as_deref().unwrap_or("(none)")
            ));
        }
    }
    Ok(())
}
