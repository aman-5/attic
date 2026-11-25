//! Production commit mode (`at_commit`): answer about a named Git commit
//! without checking it out.
//!
//! The commit's tree is written into
//! `<data>/commits/<repository_id>/<sha>/src` through a PRIVATE temporary
//! Git index (`read-tree` + `checkout-index --prefix`), so `HEAD`, the
//! user's index and working tree are never touched. That copy is indexed
//! into its own store under the SAME repository id, so every existing
//! handler (file, repo_map, context) works on it unchanged. Commit content
//! is immutable, so a commit store never needs a freshness check; cached
//! stores are bounded by count and bytes (LRU).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use attic_storage::{WriterQueue, WriterQueueHandle};

use crate::freshness::freshness_error;
use crate::*;

/// Commit stores kept open (most recently used last).
const MAX_OPEN_COMMIT_STORES: usize = 4;
/// Disk budget for all materialized commits + their indexes.
const MAX_COMMIT_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Marker written once a commit store is fully materialized and indexed.
const COMPLETE_MARKER: &str = "complete";

/// One indexed commit.
pub(crate) struct CommitStore {
    pub(crate) repository_id: String,
    pub(crate) sha: String,
    pub(crate) pool: DbPool,
    pub(crate) writer: WriterQueueHandle,
    dir: PathBuf,
    _queue: WriterQueue,
}

type Cache = Mutex<Vec<Arc<CommitStore>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

fn git(root: &Path, args: &[&str], index_file: Option<&Path>) -> Result<String, ServerError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    if let Some(index) = index_file {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output().map_err(|e| {
        freshness_error(
            "COMMIT_UNAVAILABLE",
            false,
            format!("git is not available: {e}"),
            Value::Null,
        )
    })?;
    if !out.status.success() {
        return Err(freshness_error(
            "COMMIT_UNAVAILABLE",
            false,
            format!(
                "git {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Value::Null,
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Full SHA of `rev` in the repository at `root`.
pub(crate) fn resolve_commit(root: &Path, rev: &str) -> Result<String, ServerError> {
    if rev.trim().is_empty() || rev.starts_with('-') {
        return Err(ServerError::InvalidArg(
            "at_commit must name a commit".into(),
        ));
    }
    if attic_discovery::git::find_git_root(root, root).is_none() {
        return Err(freshness_error(
            "COMMIT_UNAVAILABLE",
            false,
            "commit mode needs a Git repository; this repository is a plain folder",
            Value::Null,
        ));
    }
    git(
        root,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
        None,
    )
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map(|m| m.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

/// Delete least-recently-used commit directories (not currently open)
/// until the cache fits its byte budget.
fn enforce_disk_budget(commits_root: &Path, open: &[Arc<CommitStore>]) {
    let mut dirs: Vec<(std::time::SystemTime, PathBuf, u64)> = Vec::new();
    for repo in std::fs::read_dir(commits_root)
        .into_iter()
        .flatten()
        .flatten()
    {
        for commit in std::fs::read_dir(repo.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let path = commit.path();
            let used = std::fs::metadata(path.join(COMPLETE_MARKER))
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            dirs.push((used, path.clone(), dir_size(&path)));
        }
    }
    let mut total: u64 = dirs.iter().map(|(_, _, s)| s).sum();
    dirs.sort_by_key(|(used, _, _)| *used);
    for (_, path, size) in dirs {
        if total <= MAX_COMMIT_CACHE_BYTES {
            break;
        }
        if open.iter().any(|s| s.dir == path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

/// Open (materializing and indexing on first use) the store for `sha` of
/// the repository `repository_id` rooted at `root`.
pub(crate) fn open_commit_store(
    data_dir: &Path,
    repository_id: &str,
    root: &Path,
    sha: &str,
    policy: &attic_discovery::DiscoveryPolicy,
    options: &IndexOptions,
) -> Result<Arc<CommitStore>, ServerError> {
    let mut open = cache().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(pos) = open
        .iter()
        .position(|s| s.repository_id == repository_id && s.sha == sha)
    {
        let store = open.remove(pos);
        open.push(Arc::clone(&store));
        return Ok(store);
    }
    let commits_root = data_dir.join("commits");
    let dir = commits_root.join(repository_id).join(sha);
    let src = dir.join("src");
    let db = dir.join("index.db");
    // A store built by an older schema would be reset to empty by the
    // migration below while its marker still says "complete": rebuild it.
    let complete = dir.join(COMPLETE_MARKER).is_file()
        && attic_storage::connection::open_ro(&db)
            .ok()
            .is_some_and(|c| attic_storage::schema_is_current(&c).unwrap_or(false));
    if !complete {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&src)?;
        let index_file = dir.join("git-index");
        git(root, &["read-tree", sha], Some(&index_file))?;
        let prefix = format!("{}/", src.to_string_lossy().replace('\\', "/"));
        git(
            root,
            &["checkout-index", "-a", "-f", &format!("--prefix={prefix}")],
            Some(&index_file),
        )?;
        let _ = std::fs::remove_file(&index_file);
    }
    let (conn, pool) = attic_storage::open_db(&db)?;
    attic_storage::run_migrations(&conn)?;
    let queue = WriterQueue::new(conn)?;
    let writer = queue.handle();
    if !complete {
        let typed: attic_core::RepositoryId = repository_id.parse().map_err(|_| {
            ServerError::InvalidArg(format!("invalid repository id {repository_id}"))
        })?;
        let src_root = src.to_string_lossy().to_string();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        writer.send(move |c| {
            attic_storage::upsert_repository(c, &typed, &src_root, &name)?;
            Ok(())
        })?;
        let store = IndexingStore {
            readers: &pool,
            writer: &writer,
        };
        attic_indexing::index_repository(&store, &src, policy, options)?;
        std::fs::write(dir.join(COMPLETE_MARKER), sha)?;
    } else {
        // Touch the marker: LRU order for the disk budget.
        let _ = std::fs::write(dir.join(COMPLETE_MARKER), sha);
    }
    let store = Arc::new(CommitStore {
        repository_id: repository_id.to_owned(),
        sha: sha.to_owned(),
        pool,
        writer,
        dir,
        _queue: queue,
    });
    open.push(Arc::clone(&store));
    while open.len() > MAX_OPEN_COMMIT_STORES {
        open.remove(0);
    }
    enforce_disk_budget(&commits_root, &open);
    Ok(store)
}

/// Parsed `at_commit` argument: `{repository_id → revision}`.
pub(crate) fn parse_at_commit(
    args: &HashMap<String, Value>,
) -> Result<Option<BTreeMap<String, String>>, ServerError> {
    let Some(raw) = args.get("at_commit") else {
        return Ok(None);
    };
    match raw {
        Value::Null => Ok(None),
        Value::String(rev) => {
            let id = args
                .get("repository_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ServerError::InvalidArg(
                        "at_commit as a string needs repository_id/repository; use {repository_id: commit} for several repositories".into(),
                    )
                })?;
            Ok(Some(BTreeMap::from([(id.to_owned(), rev.clone())])))
        }
        Value::Object(map) if !map.is_empty() => map
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_owned()))
                    .ok_or_else(|| {
                        ServerError::InvalidArg("at_commit values must be commit strings".into())
                    })
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Some),
        _ => Err(ServerError::InvalidArg(
            "at_commit must be a commit string or {repository_id: commit}".into(),
        )),
    }
}

impl AtticServer {
    /// Answer `tool` from pinned commits instead of the working tree.
    pub(crate) fn commit_mode_tool(
        &self,
        tool: &str,
        args: &HashMap<String, Value>,
        active_ids: &HashSet<String>,
        commits: &BTreeMap<String, String>,
        advisory: attic_storage::resource_manager::ResourceAdvisory,
    ) -> Result<CallToolResult, ServerError> {
        if tool != "context" && commits.len() != 1 {
            return Err(ServerError::InvalidArg(
                "file and repo_map take one repository in at_commit".into(),
            ));
        }
        let data_dir = self
            .db_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let mut stores = Vec::new();
        for (id, rev) in commits {
            require_active_member(active_ids, id)?;
            let local = self.repository_server(id)?;
            let typed: attic_core::RepositoryId = id
                .parse()
                .map_err(|_| ServerError::InvalidArg(format!("invalid repository id {id}")))?;
            let root = local
                .pool
                .with_reader(|c| attic_storage::get_repository_path(c, &typed))?
                .ok_or_else(|| {
                    ServerError::InvalidArg(format!("repository {id} is not indexed"))
                })?;
            let root = PathBuf::from(root);
            let sha = resolve_commit(&root, rev)?;
            stores.push(open_commit_store(
                &data_dir,
                id,
                &root,
                &sha,
                &self.discovery_policy(),
                &self.index_options(),
            )?);
        }
        let mut token_repos = BTreeMap::new();
        let mut repositories = Vec::new();
        for s in &stores {
            token_repos.insert(
                s.repository_id.clone(),
                attic_storage::StoreVersion {
                    epoch: format!("commit:{}", s.sha),
                    version: 0,
                },
            );
            repositories.push(json!({
                "repository_id": s.repository_id,
                "source_label": {"kind": "commit", "commit": s.sha, "branch": null},
                "verification": {"status": "commit", "note": "content read from the named commit; the working tree was not used"},
            }));
        }
        let token = crate::freshness::VersionToken {
            v: 1,
            repos: token_repos,
            catalog: None,
        };
        let freshness = json!({
            "version_token": token.encode(),
            "repositories": repositories,
            "unavailable_repositories": [],
        });

        let mut local_args = args.clone();
        local_args.remove("at_commit");
        let result = if stores.len() == 1 {
            let s = &stores[0];
            local_args.insert("repository_id".into(), json!(s.repository_id));
            let only = HashSet::from([s.repository_id.clone()]);
            match tool {
                "file" => handle_file(&s.pool, &local_args, &only)?,
                "repo_map" => handle_repo_map(
                    &s.pool,
                    &local_args,
                    &only,
                    &HashMap::new(),
                    &HashMap::new(),
                )?,
                _ => handle_context(&s.pool, &s.writer, true, &local_args, &only, advisory, None)?,
            }
        } else {
            self.commit_context_multi(&stores, &local_args, advisory)?
        };
        Ok(crate::freshness::attach_freshness(result, &freshness))
    }

    fn commit_context_multi(
        &self,
        stores: &[Arc<CommitStore>],
        args: &HashMap<String, Value>,
        advisory: attic_storage::resource_manager::ResourceAdvisory,
    ) -> Result<CallToolResult, ServerError> {
        let diagnostics = crate::handlers::context::include_diagnostics(args)?;
        let mut outputs = Vec::new();
        let mut documents = Vec::new();
        for s in stores {
            let mut local = args.clone();
            local.insert("repository_id".into(), json!(s.repository_id));
            let only = HashSet::from([s.repository_id.clone()]);
            let (answer, document) = crate::handlers::context::prepare_context(
                &s.pool, &s.writer, true, &local, &only, advisory, None,
            )?;
            if let Some(document) = document {
                documents.push(document);
            }
            outputs.push(json!({"repository_id": s.repository_id, "answer": answer}));
        }
        // Temporary cross-repository catalog for exactly these commits.
        let conns: Vec<rusqlite::Connection> = stores
            .iter()
            .map(|s| attic_storage::connection::open_ro(&s.dir.join("index.db")))
            .collect::<Result<Vec<_>, _>>()?;
        let pairs: Vec<(&rusqlite::Connection, String)> = conns
            .iter()
            .zip(stores)
            .map(|(c, s)| (c, s.repository_id.clone()))
            .collect();
        let (edges, resolution) = attic_crossrepo::maintenance::resolve_in_memory(&pairs)
            .map_err(|e| ServerError::Retrieval(e.to_string()))?;
        let mut payload = crate::handlers::context::aggregate_context_answers(
            &outputs,
            &documents,
            &[],
            stores.len(),
            diagnostics,
        )?;
        payload["commit_links"] = json!(
            edges
                .iter()
                .map(|e| json!({
                    "source_repository_id": e.source_repository_id,
                    "target_repository_id": e.target_repository_id,
                    "dependency_basis": e.dependency_basis,
                    "resolution": e.resolution,
                    "confidence": e.confidence,
                    "version_compat": serde_json::from_str::<Value>(&e.provenance_json)
                        .ok()
                        .and_then(|p| p.get("version_compat").cloned()),
                }))
                .collect::<Vec<_>>()
        );
        payload["commit_link_diagnostics"] = json!({
            "unresolved": resolution
                .missing_targets
                .iter()
                .filter(|m| !resolution.external_unmatched.contains(m))
                .collect::<Vec<_>>(),
            "ambiguous": resolution.ambiguous_targets,
        });
        crate::handlers::context::context_response_bounded(
            payload,
            crate::handlers::context::response_limit(args),
        )
    }
}
