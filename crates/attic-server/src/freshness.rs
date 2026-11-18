//! Server side of the freshness gate.
//!
//! Every index-reading tool call runs [`AtticServer::gate_repositories`]
//! for the repositories it will read BEFORE reading any index data (R1),
//! and [`AtticServer::versions_unchanged`] AFTER reading: every index write
//! bumps a store's persisted version, so an unchanged version proves the
//! reads saw exactly the checked data (R3, detect-and-refuse). A changed
//! version means the answer may mix versions and is refused with
//! `VERSION_EXPIRED` (retryable) after one internal retry.

use std::collections::BTreeMap;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use attic_incremental::{ExpectedSource, GateConfig, GateOutcome, IncrementalError};
use attic_storage::StoreVersion;

use crate::*;

/// Maximum time one tool call waits for the freshness check. A check that
/// runs longer keeps going in the background (holding the repository lock)
/// and the next call joins it.
pub(crate) const GATE_DEADLINE: Duration = Duration::from_secs(30);

/// Uncertain paths listed per repository in a response (count is exact).
const MAX_LISTED_UNCERTAIN: usize = 20;

/// Repositories a single-store tool call reads: the explicit
/// `repository_id`, else every active repository.
pub(crate) fn gate_scope_ids(
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Vec<String> {
    match args.get("repository_id").and_then(Value::as_str) {
        Some(id) => vec![id.to_owned()],
        None => {
            let mut ids: Vec<String> = active_ids.iter().cloned().collect();
            ids.sort();
            ids
        }
    }
}

/// Render a structured freshness error.
pub(crate) fn error_json(code: &str, retryable: bool, message: &str, detail: &Value) -> String {
    let mut v = json!({
        "code": code,
        "retryable": retryable,
        "message": message,
    });
    if !detail.is_null() {
        v["detail"] = detail.clone();
    }
    v.to_string()
}

pub(crate) fn freshness_error(
    code: &'static str,
    retryable: bool,
    message: impl Into<String>,
    detail: Value,
) -> ServerError {
    ServerError::Freshness {
        code,
        retryable,
        message: message.into(),
        detail,
    }
}

/// Opaque version token returned with every gated answer and accepted back
/// as `version_token` so a multi-call answer can prove it read one version.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct VersionToken {
    pub(crate) v: u32,
    pub(crate) repos: BTreeMap<String, StoreVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) catalog: Option<String>,
}

impl VersionToken {
    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub(crate) fn parse(args: &HashMap<String, Value>) -> Result<Option<Self>, ServerError> {
        let Some(raw) = args.get("version_token") else {
            return Ok(None);
        };
        let Some(text) = raw.as_str() else {
            return Err(ServerError::InvalidArg(
                "version_token must be the string returned by a previous call".into(),
            ));
        };
        let token: Self = serde_json::from_str(text).map_err(|_| {
            ServerError::InvalidArg("version_token is not a token returned by Attic".into())
        })?;
        Ok(Some(token))
    }
}

pub(crate) fn expected_source(
    args: &HashMap<String, Value>,
) -> Result<ExpectedSource, ServerError> {
    let get = |key: &str| -> Result<Option<String>, ServerError> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_owned())),
            Some(_) => Err(ServerError::InvalidArg(format!(
                "{key} must be a non-empty string"
            ))),
        }
    };
    Ok(ExpectedSource {
        branch: get("expected_branch")?,
        commit: get("expected_commit")?,
    })
}

/// One repository that passed the gate.
#[derive(Debug, Clone)]
pub(crate) struct RepoPass {
    pub(crate) id: String,
    pub(crate) outcome: GateOutcome,
}

/// Result of gating a set of repositories.
#[derive(Debug, Clone, Default)]
pub(crate) struct GatePass {
    pub(crate) repos: Vec<RepoPass>,
    /// Repositories that could not be verified and were NOT read:
    /// `{repository_id, code, message}`.
    pub(crate) unavailable: Vec<Value>,
}

impl GatePass {
    pub(crate) fn passed_ids(&self) -> Vec<String> {
        self.repos.iter().map(|r| r.id.clone()).collect()
    }

    pub(crate) fn token(
        &self,
        prior: Option<&VersionToken>,
        catalog: Option<String>,
    ) -> VersionToken {
        let mut repos = prior.map(|t| t.repos.clone()).unwrap_or_default();
        for r in &self.repos {
            repos.insert(r.id.clone(), r.outcome.version.clone());
        }
        VersionToken {
            v: 1,
            repos,
            catalog: catalog.or_else(|| prior.and_then(|t| t.catalog.clone())),
        }
    }

    /// The `freshness` block attached to every gated response.
    pub(crate) fn to_json(&self, token: &VersionToken) -> Value {
        json!({
            "version_token": token.encode(),
            "repositories": self.repos.iter().map(|r| json!({
                "repository_id": r.id,
                "source_label": r.outcome.source_label,
                "verification": {
                    "status": if r.outcome.full_hash { "full_hash" } else { "verified" },
                    "checked_at_ms": chrono_ms(),
                    "elapsed_ms": r.outcome.elapsed_ms,
                    "files_hashed": r.outcome.files_hashed,
                    "files_unchanged_by_metadata": r.outcome.files_unchanged_by_metadata,
                    "files_reindexed": r.outcome.files_reindexed,
                    "uncertain_file_count": r.outcome.uncertain.len(),
                    "uncertain_files": r.outcome.uncertain.iter().take(MAX_LISTED_UNCERTAIN).collect::<Vec<_>>(),
                },
            })).collect::<Vec<_>>(),
            "unavailable_repositories": self.unavailable,
        })
    }
}

fn chrono_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
enum GateFailure {
    NotReady(String),
    Unavailable(String),
    Changing(String),
    Other(String),
}

/// One freshness check for one repository, shared by every caller that
/// joined it.
struct Flight {
    result: Mutex<Option<Result<GateOutcome, GateFailure>>>,
    done: Condvar,
}

impl Flight {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            done: Condvar::new(),
        })
    }

    fn finish(&self, r: Result<GateOutcome, GateFailure>) {
        *self.result.lock().unwrap_or_else(|p| p.into_inner()) = Some(r);
        self.done.notify_all();
    }

    fn wait(&self, timeout: Duration) -> Option<Result<GateOutcome, GateFailure>> {
        let guard = self.result.lock().unwrap_or_else(|p| p.into_inner());
        let (guard, _) = self
            .done
            .wait_timeout_while(guard, timeout, |r| r.is_none())
            .unwrap_or_else(|p| p.into_inner());
        guard.clone()
    }
}

/// Per repository (and deep_verify flag): the running check and at most one
/// queued follow-up. A caller never joins a check that started before it
/// arrived (it could miss the caller's changes): it joins the queued
/// follow-up, which starts after the running one finishes. So retries after
/// a TIMEOUT share work instead of piling up full checks.
#[derive(Default)]
struct RepoFlights {
    running: Option<Arc<Flight>>,
    queued: Option<Arc<Flight>>,
}

fn flights() -> &'static Mutex<HashMap<(String, bool), RepoFlights>> {
    static FLIGHTS: OnceLock<Mutex<HashMap<(String, bool), RepoFlights>>> = OnceLock::new();
    FLIGHTS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl AtticServer {
    /// Run the freshness gate for `ids` (in parallel, bounded by
    /// [`GATE_DEADLINE`]) and apply the caller's `expected_branch` /
    /// `expected_commit` and `version_token` checks.
    pub(crate) fn gate_repositories(
        &self,
        ids: &[String],
        args: &HashMap<String, Value>,
    ) -> Result<GatePass, ServerError> {
        let prior = VersionToken::parse(args)?;
        let expected = expected_source(args)?;
        let deep_verify = match args.get("deep_verify") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ServerError::InvalidArg(
                    "deep_verify must be a boolean".into(),
                ));
            }
        };
        let started = Instant::now();
        let flights: Vec<(String, Arc<Flight>)> = ids
            .iter()
            .map(|id| (id.clone(), self.join_check(id, deep_verify)))
            .collect();

        let mut pass = GatePass::default();
        let mut pending: std::collections::BTreeSet<String> = ids.iter().cloned().collect();
        for (id, flight) in flights {
            let remaining = GATE_DEADLINE.saturating_sub(started.elapsed());
            let Some(result) = flight.wait(remaining) else {
                return Err(freshness_error(
                    "TIMEOUT",
                    true,
                    "the freshness check did not finish in time; it continues in the background — retry shortly",
                    json!({"still_checking": pending}),
                ));
            };
            pending.remove(&id);
            match result {
                Ok(outcome) => pass.repos.push(RepoPass { id, outcome }),
                Err(GateFailure::NotReady(m)) => pass.unavailable.push(json!({
                    "repository_id": id, "code": "INDEX_NOT_READY", "message": m,
                })),
                Err(GateFailure::Unavailable(m)) => pass.unavailable.push(json!({
                    "repository_id": id, "code": "SOURCE_UNAVAILABLE", "message": m,
                })),
                Err(GateFailure::Changing(m)) => {
                    return Err(freshness_error(
                        "SOURCE_CHANGING",
                        true,
                        format!("files kept changing during the freshness check: {m}"),
                        json!({"repository_id": id}),
                    ));
                }
                Err(GateFailure::Other(m)) => {
                    return Err(ServerError::Retrieval(format!(
                        "freshness check failed for {id}: {m}"
                    )));
                }
            }
        }
        pass.repos.sort_by(|a, b| a.id.cmp(&b.id));

        for r in &pass.repos {
            if let Err(why) = attic_incremental::check_expected(&r.outcome.source_label, &expected)
            {
                return Err(freshness_error(
                    "BRANCH_MISMATCH",
                    false,
                    why,
                    json!({
                        "repository_id": r.id,
                        "source_label": r.outcome.source_label,
                    }),
                ));
            }
        }
        if let Some(prior) = &prior {
            for r in &pass.repos {
                if let Some(pinned) = prior.repos.get(&r.id)
                    && pinned != &r.outcome.version
                {
                    return Err(freshness_error(
                        "VERSION_EXPIRED",
                        true,
                        "the code changed since the version_token was issued; restart the answer without version_token",
                        json!({"repository_id": r.id}),
                    ));
                }
            }
        }
        if pass.repos.is_empty()
            && let Some(first) = pass.unavailable.first()
        {
            let code = if first["code"] == "INDEX_NOT_READY" {
                "INDEX_NOT_READY"
            } else {
                "SOURCE_UNAVAILABLE"
            };
            return Err(freshness_error(
                code,
                code == "INDEX_NOT_READY",
                "no requested repository could be verified",
                json!({"unavailable_repositories": pass.unavailable}),
            ));
        }
        Ok(pass)
    }

    /// Join (or start) the freshness check for `id` (see [`RepoFlights`]).
    fn join_check(&self, id: &str, deep_verify: bool) -> Arc<Flight> {
        let key = (id.to_owned(), deep_verify);
        let mut map = flights().lock().unwrap_or_else(|p| p.into_inner());
        let entry = map.entry(key.clone()).or_default();
        let (flight, wait_for) = match (&entry.running, &entry.queued) {
            (None, _) => {
                let f = Flight::new();
                entry.running = Some(Arc::clone(&f));
                (f, None)
            }
            (Some(_), Some(q)) => return Arc::clone(q),
            (Some(r), None) => {
                let f = Flight::new();
                entry.queued = Some(Arc::clone(&f));
                (f, Some(Arc::clone(r)))
            }
        };
        drop(map);
        let server = self.clone();
        let run = Arc::clone(&flight);
        // Detached: a check that outlives a caller's deadline keeps running
        // and later callers join it.
        std::thread::spawn(move || {
            if let Some(prev) = wait_for {
                while prev.wait(Duration::from_secs(3600)).is_none() {}
                let mut map = flights().lock().unwrap_or_else(|p| p.into_inner());
                let entry = map.entry(key.clone()).or_default();
                entry.running = Some(Arc::clone(&run));
                if entry.queued.as_ref().is_some_and(|q| Arc::ptr_eq(q, &run)) {
                    entry.queued = None;
                }
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                server.gate_one(&key.0, key.1)
            }))
            .unwrap_or_else(|_| Err(GateFailure::Other("freshness check panicked".into())));
            run.finish(result);
            let mut map = flights().lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = map.get_mut(&key)
                && entry.running.as_ref().is_some_and(|r| Arc::ptr_eq(r, &run))
            {
                entry.running = None;
            }
        });
        flight
    }

    fn gate_one(&self, id: &str, deep_verify: bool) -> Result<GateOutcome, GateFailure> {
        let local = self
            .repository_server(id)
            .map_err(|e| GateFailure::NotReady(e.to_string()))?;
        let typed: attic_core::RepositoryId = id
            .parse()
            .map_err(|_| GateFailure::Other(format!("invalid repository id {id}")))?;
        let root = local
            .pool
            .with_reader(|c| attic_storage::get_repository_path(c, &typed))
            .map_err(|e| GateFailure::Other(e.to_string()))?
            .ok_or_else(|| GateFailure::NotReady("repository is not indexed yet".into()))?;
        // The initial full index is still running for this root: answers
        // wait for it rather than racing it.
        let key = root_identity_key(Path::new(&root));
        if self.bootstrap_jobs.lock().is_ok_and(|jobs| {
            jobs.iter()
                .any(|job| job.root_key == key && !job.handle.is_finished())
        }) {
            return Err(GateFailure::NotReady(
                "repository is still being indexed".into(),
            ));
        }
        let config = GateConfig {
            index_options: self.index_options(),
            always_hash: deep_verify || self.attic_config.freshness.always_hash(Path::new(&root)),
            ..Default::default()
        };
        attic_incremental::ensure_current(
            &local.pool,
            &local.writer,
            Path::new(&root),
            &self.discovery_policy(),
            &config,
        )
        .map_err(|e| match e {
            IncrementalError::NotBootstrapped(m) => GateFailure::NotReady(m),
            IncrementalError::SourceUnavailable(m) => GateFailure::Unavailable(m),
            IncrementalError::SourceChanging(m) => GateFailure::Changing(m),
            other => GateFailure::Other(other.to_string()),
        })
    }

    /// Single-store mode (R4): rebuild cross-repository links in the shared
    /// store when the catalog version of `active_ids` changed. Returns
    /// `true` when the links match the gated indexes.
    pub(crate) fn ensure_single_store_catalog(
        &self,
        active_ids: &HashSet<String>,
    ) -> Result<bool, ServerError> {
        let mut ids: Vec<String> = active_ids.iter().cloned().collect();
        ids.sort();
        let stale = self.pool.with_reader(|conn| {
            use rusqlite::OptionalExtension;
            let version = attic_crossrepo::catalog::catalog_version(conn, &ids)
                .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
            let latest: Option<String> = conn
                .query_row(
                    "SELECT catalog_version FROM core_workspace_snapshots
                      ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(latest.as_deref() != Some(version.as_str()))
        })?;
        if stale {
            self.pool.with_reader(|conn| {
                attic_crossrepo::maintenance::sync_workspace(
                    conn,
                    &self.writer,
                    &attic_crossrepo::maintenance::WorkspaceSyncOptions {
                        active_repository_ids: Some(ids.clone()),
                        ..Default::default()
                    },
                )
                .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
            })?;
        }
        Ok(true)
    }

    /// After-read check (R3): `true` when no index write committed in any
    /// gated repository since its gate pass.
    pub(crate) fn versions_unchanged(&self, pass: &GatePass) -> Result<bool, ServerError> {
        for r in &pass.repos {
            let local = self.repository_server(&r.id)?;
            let now = local.pool.with_reader(attic_storage::read_store_version)?;
            if now.as_ref() != Some(&r.outcome.version) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Gate `ids`, run `read` over the repositories that passed, and verify
    /// no index write landed in between; retries once, then refuses with
    /// `VERSION_EXPIRED`. Attaches the `freshness` block to the payload.
    pub(crate) fn gated_read(
        &self,
        ids: &[String],
        args: &HashMap<String, Value>,
        mut read: impl FnMut(&GatePass) -> Result<CallToolResult, ServerError>,
    ) -> Result<CallToolResult, ServerError> {
        let prior = VersionToken::parse(args)?;
        for attempt in 0..2 {
            let pass = self.gate_repositories(ids, args)?;
            let result = match read(&pass) {
                Err(ServerError::Freshness {
                    code: "VERSION_EXPIRED",
                    ..
                }) if attempt == 0 => continue,
                other => other?,
            };
            if !self.versions_unchanged(&pass)? {
                if attempt == 0 {
                    continue;
                }
                return Err(freshness_error(
                    "VERSION_EXPIRED",
                    true,
                    "the index changed while the answer was being read; retry",
                    Value::Null,
                ));
            }
            let token = pass.token(prior.as_ref(), None);
            return Ok(attach_freshness(result, &pass.to_json(&token)));
        }
        unreachable!("loop returns on its second iteration")
    }
}

/// Add `freshness` to a successful JSON tool result (non-JSON / error
/// results are returned unchanged).
pub(crate) fn attach_freshness(result: CallToolResult, freshness: &Value) -> CallToolResult {
    if result.is_error == Some(true) {
        return result;
    }
    let text: String = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n");
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(mut map)) => {
            map.insert("freshness".into(), freshness.clone());
            let payload = Value::Object(map);
            // Responses with a byte budget are re-bounded so the freshness
            // block counts against the limit and telemetry stays exact.
            if let Some(max) = payload["response_budget"]["max_bytes"].as_u64() {
                if let Ok(bounded) = crate::handlers::context::context_response_bounded(
                    payload.clone(),
                    max as usize,
                ) {
                    return bounded;
                }
                // Too large even after trimming results: keep only the
                // token and counts (the answer still proves its version).
                let mut compact = payload;
                compact["freshness"] = json!({
                    "version_token": freshness["version_token"],
                    "repository_count": freshness["repositories"].as_array().map_or(0, Vec::len),
                    "unavailable_repositories": freshness["unavailable_repositories"],
                    "detail_omitted": "response_limit",
                });
                return crate::handlers::context::context_response_bounded(compact, max as usize)
                    .unwrap_or(result);
            }
            CallToolResult::success(vec![ContentBlock::text(payload.to_string())])
        }
        _ => {
            // Plain-text tools (file): freshness travels as a second block.
            let mut content = result.content.clone();
            content.push(ContentBlock::text(
                json!({"freshness": freshness}).to_string(),
            ));
            CallToolResult::success(content)
        }
    }
}

/// How a live file relates to its indexed version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileConsistency {
    /// No present occurrence: excluded, ignored, or new — served live and
    /// labelled `file_indexed: false`.
    NotIndexed,
    /// Live bytes are the indexed bytes.
    Matches,
    /// The file changed after the gate checked it.
    Differs,
}

/// Files larger than this are compared by stored metadata, not re-hashed.
const FILE_HASH_LIMIT: u64 = 1024 * 1024;

/// `(content_hash, existence_state, size_bytes, mtime_ns, change_time_ns,
/// file_id)` of a path's latest occurrence.
type OccurrenceFacts = (
    String,
    String,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// p3 file contract: compare the live file with the version the gate
/// verified.
pub(crate) fn file_consistency(
    pool: &DbPool,
    repo_id: &str,
    rel: &str,
) -> Result<FileConsistency, ServerError> {
    let typed: attic_core::RepositoryId = repo_id
        .parse()
        .map_err(|_| ServerError::InvalidArg(format!("invalid repository id {repo_id}")))?;
    let rel = rel.replace('\\', "/");
    let Some(root) = pool.with_reader(|c| attic_storage::get_repository_path(c, &typed))? else {
        return Ok(FileConsistency::NotIndexed);
    };
    let row: Option<OccurrenceFacts> = pool.with_reader(|c| {
        use rusqlite::OptionalExtension;
        Ok(c.query_row(
            "SELECT fo.content_hash, fo.existence_state, fo.size_bytes,
                        fo.mtime_ns, fo.change_time_ns, fo.file_id
                   FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1 AND fo.path = ?2
                  ORDER BY fo.occurrence_seq DESC LIMIT 1",
            rusqlite::params![repo_id, rel],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?)
    })?;
    let Some((hash, existence, size, mtime, ctime, file_id)) = row else {
        return Ok(FileConsistency::NotIndexed);
    };
    if existence == "deleted" {
        return Ok(FileConsistency::NotIndexed);
    }
    let abs = Path::new(&root).join(&rel);
    let Ok(meta) = attic_discovery::file_meta(&abs) else {
        return Ok(FileConsistency::Differs);
    };
    if meta.size > FILE_HASH_LIMIT {
        let same = u64::try_from(size).ok() == Some(meta.size)
            && mtime == Some(meta.mtime_ns)
            && ctime.is_some()
            && ctime == meta.change_time_ns
            && file_id.is_some()
            && file_id == meta.file_id;
        return Ok(if same {
            FileConsistency::Matches
        } else {
            FileConsistency::Differs
        });
    }
    let bytes = std::fs::read(&abs)?;
    Ok(if blake3::hash(&bytes).to_hex().as_str() == hash {
        FileConsistency::Matches
    } else {
        FileConsistency::Differs
    })
}

/// Run the `file` tool under the p3 contract.
pub(crate) fn gated_file_read(
    pool: &DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
) -> Result<CallToolResult, ServerError> {
    let result = handle_file(pool, args, active_ids)?;
    let (Some(repo_id), Some(path)) = (
        args.get("repository_id").and_then(Value::as_str),
        args.get("path").and_then(Value::as_str),
    ) else {
        return Ok(result);
    };
    match file_consistency(pool, repo_id, path)? {
        FileConsistency::Matches => Ok(result),
        FileConsistency::NotIndexed => {
            let mut content = result.content.clone();
            content.push(ContentBlock::text(
                json!({"file_indexed": false}).to_string(),
            ));
            Ok(CallToolResult::success(content))
        }
        FileConsistency::Differs => Err(freshness_error(
            "VERSION_EXPIRED",
            true,
            format!("{path} changed after the freshness check; retry"),
            json!({"repository_id": repo_id, "path": path}),
        )),
    }
}
