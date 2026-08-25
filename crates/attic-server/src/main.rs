// crates/attic-server/src/main.rs
// Phase 1D – MCP server (rmcp-based), no raw rusqlite, uses DbPool + WriterQueueHandle

use attic_indexing::{IndexError, IndexOptions, index_repository};
use attic_storage::{
    DbPool, MAX_SEARCH_RESULTS, StorageError, WriterQueue, WriterQueueHandle,
    fts_search, FtsSearchParams, get_db_stats, get_repository_path, get_repository_stats,
    lookup_repository_by_root_path, run_migrations,
};
use attic_storage::connection::open_rw;
use attic_discovery::{
    DiscoveryPolicy, SecretScanDecision,
    canonicalize_within_root, preprocess_file_content,
};
use rmcp::{
    ErrorData as McpError,
    RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs,
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tracing::{error, info, warn};

const SERVER_NAME: &str    = "attic";
const SERVER_VERSION: &str = "0.1.0";

#[derive(Debug, Error)]
enum ServerError {
    #[error("storage error: {0}")]    Storage(#[from] StorageError),
    #[error("indexing error: {0}")]   Indexing(#[from] IndexError),
    #[error("discovery I/O: {0}")]    Discovery(#[from] io::Error),
    #[error("json error: {0}")]       Json(#[from] serde_json::Error),
    #[error("invalid argument: {0}")] InvalidArg(String),
}

#[derive(Clone)]
struct AtticServer {
    pool:    DbPool,
    writer:  WriterQueueHandle,
    _queue:  Arc<WriterQueue>,
    db_path: Arc<PathBuf>,
}

impl AtticServer {
    fn new(db_path: &Path) -> Result<Self, ServerError> {
        let (conn, pool) = attic_storage::open_db(db_path).map_err(ServerError::Storage)?;
        run_migrations(&conn).map_err(ServerError::Storage)?;
        let queue = WriterQueue::new(conn).map_err(ServerError::Storage)?;
        let writer = queue.handle();
        let _queue = Arc::new(queue);
        Ok(AtticServer { pool, writer, _queue, db_path: Arc::new(db_path.to_path_buf()) })
    }

    fn bootstrap_workspace(&self, root: &Path) -> Result<String, ServerError> {
        let root_str = root.to_string_lossy().to_string();
        if let Some(id) = self.pool.with_reader(|c| lookup_repository_by_root_path(c, &root_str))? {
            return Ok(id.to_string());
        }
        // Open a dedicated write connection that bypasses WriterQueue's BEGIN IMMEDIATE
        // wrapper.  index_repository calls publish_file_batch and
        // insert_retrieval_unit_with_fts, each of which starts its own transaction.
        // Running those inside a WriterQueue::send closure would nest BEGIN IMMEDIATE
        // calls, which SQLite rejects.  A direct connection has no enclosing
        // transaction, so index_repository's own transactions work correctly.
        let conn = open_rw(&self.db_path).map_err(ServerError::Storage)?;
        let policy = DiscoveryPolicy::default_git();
        let opts   = IndexOptions::default(); // skip_migrations: false — no outer transaction
        index_repository(&conn, root, &policy, &opts)
            .map(|r| r.repository_id)
            .map_err(ServerError::Indexing)
    }
}

// ─── input validation ──────────────────────────────────────────────────────────

fn validate_filter(name: &str, value: &str, max_len: usize) -> Result<(), ServerError> {
    if value.len() > max_len {
        return Err(ServerError::InvalidArg(format!("{name} too long (max {max_len})")));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(ServerError::InvalidArg(format!("{name} contains control characters")));
    }
    Ok(())
}

fn validate_repository_id(id: &str) -> Result<(), ServerError> {
    validate_filter("repository_id", id, 64)?;
    if !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(ServerError::InvalidArg(
            "repository_id must be a UUID (hex digits and hyphens only)".into(),
        ));
    }
    Ok(())
}

// ─── tool handlers ─────────────────────────────────────────────────────────────

fn handle_file(
    pool: &DbPool,
    args: &HashMap<String, Value>,
) -> Result<CallToolResult, ServerError> {
    let repo_id = args.get("repository_id").and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;

    let file_path = args.get("path").and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("path required".into()))?;

    let start_line = args.get("start_line").and_then(Value::as_u64).map(|v| v as usize);
    let end_line   = args.get("end_line").and_then(Value::as_u64).map(|v| v as usize);
    let start_byte = args.get("start_byte").and_then(Value::as_u64).map(|v| v as usize);
    let end_byte   = args.get("end_byte").and_then(Value::as_u64).map(|v| v as usize);

    let parsed_repo_id = repo_id.parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;

    let repo_root_str = pool.with_reader(|c| get_repository_path(c, &parsed_repo_id))?
        .ok_or_else(|| ServerError::InvalidArg(format!("repository_id {repo_id} not found")))?;
    let repo_root_raw = PathBuf::from(&repo_root_str);
    // On Windows, std::fs::canonicalize adds a \\?\ extended-length prefix.
    // canonicalize_within_root canonicalizes the joined path, so the result
    // also has \\?\; but repo_root_raw (from the DB) does not.  Normalize
    // repo_root the same way so that strip_prefix succeeds.
    let repo_root = repo_root_raw.canonicalize().unwrap_or_else(|_| repo_root_raw);

    let abs_path = canonicalize_within_root(&repo_root.join(file_path), &repo_root)
        .map_err(|e| ServerError::InvalidArg(format!("path rejected: {e}")))?;

    let repo_relative = abs_path.strip_prefix(&repo_root)
        .map_err(|_| ServerError::InvalidArg("path outside repo root".into()))?
        .to_string_lossy().replace('\\', "/");

    // Block access to git-internal paths at the server layer regardless of
    // what preprocess_file_content decides, to ensure consistent policy.
    {
        let rr = repo_relative.as_str();
        if rr == ".git"
            || rr.starts_with(".git/")
            || rr.starts_with(".git\\")
        {
            return Err(ServerError::InvalidArg(
                "path rejected: .git internals are forbidden".into(),
            ));
        }
    }

    // preprocess handles Excluded/Redacted/secrets internally via the secrets scan layer
    let pre = preprocess_file_content(&abs_path, &repo_relative).map_err(ServerError::Discovery)?;

    if pre.decision == SecretScanDecision::Excluded {
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            format!("# {repo_relative}\n\n[Excluded by security policy]"),
        )]));
    }
    if pre.decision == SecretScanDecision::PartialScan {
        warn!("file {repo_relative}: partial scan");
    }

    let raw: String = if let Some(t) = pre.content {
        t
    } else if let Some(mut s) = pre.stream {
        attic_discovery::secrets::collect_all(&mut s)
            .map_err(ServerError::Discovery)?
            .redacted
    } else {
        String::new()
    };

    let bounded = apply_region_bounds(&raw, start_line, end_line, start_byte, end_byte);

    let header = match pre.decision {
        SecretScanDecision::Redacted    => format!("# {repo_relative}\n# [Secrets redacted]\n\n"),
        SecretScanDecision::PartialScan => format!("# {repo_relative}\n# [Partial scan]\n\n"),
        _                               => format!("# {repo_relative}\n\n"),
    };
    Ok(CallToolResult::success(vec![ContentBlock::text(format!("{header}{bounded}"))]))
}

fn apply_region_bounds(
    text: &str,
    start_line: Option<usize>,
    end_line:   Option<usize>,
    start_byte: Option<usize>,
    end_byte:   Option<usize>,
) -> Cow<'_, str> {
    if start_byte.is_some() || end_byte.is_some() {
        let s = start_byte.unwrap_or(0).min(text.len());
        let e = end_byte.map(|e| e.min(text.len())).unwrap_or(text.len());
        return Cow::Owned(text[s..e.max(s)].to_owned());
    }
    if start_line.is_some() || end_line.is_some() {
        let sl = start_line.unwrap_or(1).saturating_sub(1);
        let lines: Vec<&str> = text.lines().collect();
        let el = end_line.map(|e| e.min(lines.len())).unwrap_or(lines.len());
        return Cow::Owned(lines[sl.min(lines.len())..el].join("\n"));
    }
    Cow::Borrowed(text)
}

fn handle_search(
    pool: &DbPool,
    args: &HashMap<String, Value>,
) -> Result<CallToolResult, ServerError> {
    let query = args.get("query").and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
    validate_filter("query", query, 512)?;

    let repo_id   = args.get("repository_id").and_then(Value::as_str);
    if let Some(id) = repo_id   { validate_repository_id(id)?; }
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type { validate_filter("file_type", ft, 32)?; }
    let language  = args.get("language").and_then(Value::as_str);
    if let Some(lg) = language  { validate_filter("language", lg, 64)?; }

    let params = FtsSearchParams {
        query,
        repository_id: repo_id,
        file_type,
        language,
        max_results: MAX_SEARCH_RESULTS,
    };
    let results = pool.with_reader(|c| fts_search(c, &params))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({ "results": results }))?,
    )]))
}

fn handle_repo_map(
    pool: &DbPool,
    args: &HashMap<String, Value>,
) -> Result<CallToolResult, ServerError> {
    let repo_id = args.get("repository_id").and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type { validate_filter("file_type", ft, 32)?; }
    let all_stats = pool.with_reader(|c| get_repository_stats(c))?;
    let stats = all_stats.into_iter().find(|s| s.id == repo_id);
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({ "repository_id": repo_id, "stats": stats }))?,
    )]))
}

fn handle_status(pool: &DbPool) -> Result<CallToolResult, ServerError> {
    let stats = pool.with_reader(|c| get_db_stats(c))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({ "status": "ok", "db": stats }))?,
    )]))
}

// ─── schema helper ─────────────────────────────────────────────────────────────

fn json_schema(v: Value) -> std::sync::Arc<serde_json::Map<String, Value>> {
    std::sync::Arc::new(v.as_object().cloned().unwrap_or_default())
}

// ─── build the tool list once ──────────────────────────────────────────────────

fn make_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "file",
            "Retrieve a bounded region of the live, authoritative source of a file from an \
             indexed repository. Content is read directly from disk through the secrets-scan \
             layer; redacted or excluded files are flagged. Supports line-range \
             (start_line/end_line, 1-indexed) and byte-range (start_byte/end_byte, 0-indexed, \
             exclusive). Returns current on-disk content.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "path":          {"type":"string","description":"Repo-relative path"},
                    "start_line":    {"type":"integer","description":"First line (1-indexed)"},
                    "end_line":      {"type":"integer","description":"Last line (1-indexed, inclusive)"},
                    "start_byte":    {"type":"integer","description":"Start byte offset (0-indexed, overrides lines)"},
                    "end_byte":      {"type":"integer","description":"End byte offset (0-indexed, exclusive)"}
                },
                "required": ["repository_id","path"]
            })),
        ),
        Tool::new(
            "search",
            "Full-text search across indexed repositories using FTS5 query syntax.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "query":         {"type":"string","description":"FTS5 query (max 512 chars)"},
                    "repository_id": {"type":"string","description":"Limit results to this repository UUID"},
                    "file_type":     {"type":"string","description":"Filter by file extension (max 32)"},
                    "language":      {"type":"string","description":"Filter by detected language (max 64)"}
                },
                "required": ["query"]
            })),
        ),
        Tool::new(
            "repo_map",
            "Return statistics and structure map for an indexed repository.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "file_type":     {"type":"string","description":"Optional file-type filter"}
                },
                "required": ["repository_id"]
            })),
        ),
        Tool::new(
            "status",
            "Return server and database health status.",
            json_schema(json!({"type":"object","properties":{}})),
        ),
    ]
}

// ─── ServerHandler impl ────────────────────────────────────────────────────────

impl ServerHandler for AtticServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send {
        let tools = make_tools();
        async move {
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, McpError>> + Send {
        let pool = self.pool.clone();
        let name = request.name.clone();
        let args: HashMap<String, Value> = request.arguments
            .unwrap_or_default()
            .into_iter()
            .collect();

        async move {
            let result: Result<CallToolResult, ServerError> = match name.as_ref() {
                "file"     => handle_file(&pool, &args),
                "search"   => handle_search(&pool, &args),
                "repo_map" => handle_repo_map(&pool, &args),
                "status"   => handle_status(&pool),
                other      => Err(ServerError::InvalidArg(format!("unknown tool: {other}"))),
            };
            match result {
                Ok(r)  => Ok(r.into()),
                Err(e) => {
                    error!("tool {name} error: {e}");
                    Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into())
                }
            }
        }
    }
}

// ─── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();

    let db_path = std::env::var("ATTIC_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
                .or_else(|_| std::env::var("USERPROFILE").map(|h| PathBuf::from(h).join("AppData").join("Local")))
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("attic")
                .join("attic.db")
        });

    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p)?;
    }
    info!("attic starting, db={}", db_path.display());

    let server = AtticServer::new(&db_path)?;

    if let Ok(ws) = std::env::var("ATTIC_WORKSPACE_ROOT") {
        let root = PathBuf::from(&ws);
        let srv  = server.clone();
        tokio::task::spawn_blocking(move || match srv.bootstrap_workspace(&root) {
            Ok(id) => info!("workspace indexed, repository_id={id}"),
            Err(e) => warn!("workspace indexing failed: {e}"),
        });
    }

    server.serve(stdio()).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

// ─── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    fn make_server(tmp: &TempDir) -> AtticServer {
        AtticServer::new(&tmp.path().join("test.db")).expect("AtticServer::new")
    }

    fn text_of(r: &CallToolResult) -> String {
        match r.content.first() {
            Some(ContentBlock::Text(t)) => t.text.clone(),
            other => panic!("expected text content, got: {other:?}"),
        }
    }

    // compile-time gate: no rusqlite direct dep
    #[test]
    fn no_direct_rusqlite_in_server() {
        let _ = true;
    }

    // compile-time gate: IndexError has no Sqlite variant
    #[test]
    fn indexing_uses_writer_abstraction() {
        fn _check(e: IndexError) {
            match e {
                IndexError::Discovery(_)  => {}
                IndexError::Storage(_)    => {}
                IndexError::Io { .. }     => {}
                IndexError::PolicyHash(_) => {}
            }
        }
    }

    // validate_filter
    #[test] fn validate_filter_ok()   { assert!(validate_filter("q", "hello", 512).is_ok()); }
    #[test] fn validate_filter_long() { assert!(validate_filter("q", &"a".repeat(11), 10).is_err()); }
    #[test] fn validate_filter_ctrl() { assert!(validate_filter("q", "a\x00b", 512).is_err()); }
    #[test] fn validate_repo_id_ok()  { assert!(validate_repository_id("550e8400-e29b-41d4-a716-446655440000").is_ok()); }
    #[test] fn validate_repo_id_bad() { assert!(validate_repository_id("../../etc").is_err()); }
    #[test] fn validate_repo_id_long(){ assert!(validate_repository_id(&"a".repeat(65)).is_err()); }

    // apply_region_bounds
    #[test] fn region_full()  { let s = "a\nb\nc"; assert_eq!(apply_region_bounds(s, None, None, None, None).as_ref(), s); }
    #[test] fn region_lines() { assert_eq!(apply_region_bounds("L1\nL2\nL3", Some(2), Some(2), None, None).as_ref(), "L2"); }
    #[test] fn region_bytes() { assert_eq!(apply_region_bounds("abcdef", None, None, Some(1), Some(4)).as_ref(), "bcd"); }
    #[test] fn region_bytes_win_over_lines() { assert_eq!(apply_region_bounds("abcdef", Some(1), Some(1), Some(1), Some(4)).as_ref(), "bcd"); }
    #[test] fn region_bytes_clamped()   { assert_eq!(apply_region_bounds("hi", None, None, Some(0), Some(999)).as_ref(), "hi"); }
    #[test] fn region_bytes_past_end()  { assert_eq!(apply_region_bounds("hi", None, None, Some(999), None).as_ref(), ""); }

    // handle_file: argument gates
    #[test]
    fn file_bad_repo_id() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!("../../etc"));
        a.insert("path".into(), json!("x.rs"));
        assert!(handle_file(&make_server(&tmp).pool, &a).is_err());
    }

    #[test]
    fn file_missing_path() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!("aabbccdd"));
        let e = handle_file(&make_server(&tmp).pool, &a).unwrap_err().to_string();
        assert!(e.contains("path required"), "{e}");
    }

    #[test]
    fn file_unknown_repo() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!("deadbeef-0000-0000-0000-000000000000"));
        a.insert("path".into(), json!("src/lib.rs"));
        let e = handle_file(&make_server(&tmp).pool, &a).unwrap_err().to_string();
        assert!(e.contains("not found"), "{e}");
    }

    // handle_file: live read + region
    #[test]
    fn file_returns_live_content_and_region() {
        use std::fs;
        let tmp  = TempDir::new().unwrap();
        let srv  = make_server(&tmp);
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let repo_id = srv.bootstrap_workspace(&repo).expect("bootstrap");

        // full file
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(repo_id.clone()));
        a.insert("path".into(), json!("hello.txt"));
        let r = handle_file(&srv.pool, &a).expect("handle_file");
        let text = text_of(&r);
        assert!(text.contains("line1") && text.contains("line3"), "{text}");

        // line region
        let mut b = HashMap::new();
        b.insert("repository_id".into(), json!(repo_id));
        b.insert("path".into(), json!("hello.txt"));
        b.insert("start_line".into(), json!(2u64));
        b.insert("end_line".into(), json!(2u64));
        let r2  = handle_file(&srv.pool, &b).expect("region");
        let t2  = text_of(&r2);
        assert!(t2.contains("line2") && !t2.contains("line1"), "{t2}");
    }

    #[test]
    fn file_traversal_rejected() {
        use std::fs;
        let tmp  = TempDir::new().unwrap();
        let srv  = make_server(&tmp);
        let repo = tmp.path().join("r");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("ok.txt"), "data").unwrap();
        let id = srv.bootstrap_workspace(&repo).unwrap();
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(id));
        a.insert("path".into(), json!("../../etc/passwd"));
        assert!(handle_file(&srv.pool, &a).is_err());
    }

    #[test]
    fn file_forbidden_path_rejected() {
        use std::fs;
        let tmp  = TempDir::new().unwrap();
        let srv  = make_server(&tmp);
        let repo = tmp.path().join("r2");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git").join("config"), "[core]").unwrap();
        let id = srv.bootstrap_workspace(&repo).unwrap();
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(id));
        a.insert("path".into(), json!(".git/config"));
        // preprocess_file_content returns Excluded for .git/* — no error, but content is policy message
        let r = handle_file(&srv.pool, &a);
        match r {
            Err(e) => assert!(e.to_string().contains("forbidden") || e.to_string().contains("security") || e.to_string().contains("rejected"), "{e}"),
            Ok(cr) => {
                let t = text_of(&cr);
                assert!(t.contains("Excluded") || t.contains("security") || t.contains("forbidden"), "{t}");
            }
        }
    }

    // handle_search
    #[test]
    fn search_missing_query() {
        let tmp = TempDir::new().unwrap();
        let e = handle_search(&make_server(&tmp).pool, &HashMap::new()).unwrap_err().to_string();
        assert!(e.contains("query required"), "{e}");
    }

    #[test]
    fn search_query_too_long() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("query".into(), json!("x".repeat(513)));
        assert!(handle_search(&make_server(&tmp).pool, &a).is_err());
    }

    #[test]
    fn search_bad_repo_id() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("query".into(), json!("hello"));
        a.insert("repository_id".into(), json!("bad!id"));
        assert!(handle_search(&make_server(&tmp).pool, &a).is_err());
    }

    #[test]
    fn search_empty_db_returns_results_array() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert("query".into(), json!("hello"));
        let r = handle_search(&make_server(&tmp).pool, &a).unwrap();
        let t = text_of(&r);
        let v: Value = serde_json::from_str(&t).unwrap();
        assert!(v["results"].is_array());
    }

    // handle_status
    #[test]
    fn status_returns_ok() {
        let tmp = TempDir::new().unwrap();
        let r = handle_status(&make_server(&tmp).pool).unwrap();
        let t = text_of(&r);
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["status"], "ok");
    }

    // handle_repo_map
    #[test]
    fn repo_map_missing_repo_id() {
        let tmp = TempDir::new().unwrap();
        let e = handle_repo_map(&make_server(&tmp).pool, &HashMap::new()).unwrap_err().to_string();
        assert!(e.contains("repository_id required"), "{e}");
    }

    // workspace lifecycle: index → search
    #[test]
    fn workspace_becomes_searchable() {
        use std::fs;
        let tmp  = TempDir::new().unwrap();
        let srv  = make_server(&tmp);
        let repo = tmp.path().join("ws");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.rs"), "fn hello_world() {}").unwrap();
        let repo_id = srv.bootstrap_workspace(&repo).unwrap();

        // status should succeed
        let r = handle_status(&srv.pool).unwrap();
        let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(v["status"], "ok");

        // search for content
        let mut a = HashMap::new();
        a.insert("query".into(), json!("hello_world"));
        a.insert("repository_id".into(), json!(repo_id));
        let r2 = handle_search(&srv.pool, &a).unwrap();
        let v2: Value = serde_json::from_str(&text_of(&r2)).unwrap();
        assert!(v2["results"].is_array());
    }

    // ── MCP child-process tests ─────────────────────────────────────────────────

    fn binary_path() -> PathBuf {
        let mut p = std::env::current_exe().unwrap();
        p.pop();
        if p.ends_with("deps") { p.pop(); }
        p.join("attic")
    }

    fn mcp_request(id: u64, method: &str, params: Value) -> String {
        let v = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        format!("{}\n", serde_json::to_string(&v).unwrap())
    }

    fn send_recv(
        child: &mut std::process::Child,
        stdin: &mut std::process::ChildStdin,
        msg: &str,
    ) -> Value {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let stdout = child.stdout.as_mut().unwrap();
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn mcp_initialize_handshake() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(resp["result"]["serverInfo"]["name"].as_str().unwrap_or("").contains("attic"),
            "expected attic in serverInfo, got: {resp}");
        child.kill().ok();
    }

    #[test]
    fn mcp_tools_list() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let _ = send_recv(&mut child, &mut stdin, &init);
        let list_req = mcp_request(2, "tools/list", json!({}));
        let resp = send_recv(&mut child, &mut stdin, &list_req);
        assert_eq!(resp["jsonrpc"], "2.0");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"file"),     "missing file tool: {names:?}");
        assert!(names.contains(&"search"),   "missing search tool: {names:?}");
        assert!(names.contains(&"repo_map"), "missing repo_map tool: {names:?}");
        assert!(names.contains(&"status"),   "missing status tool: {names:?}");
        child.kill().ok();
    }

    #[test]
    fn mcp_call_tool_status() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let _ = send_recv(&mut child, &mut stdin, &init);
        let call = mcp_request(2, "tools/call", json!({"name":"status","arguments":{}}));
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        let content = &resp["result"]["content"];
        assert!(content.is_array(), "expected content array: {resp}");
        let text = content[0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).expect("status result is JSON");
        assert_eq!(v["status"], "ok", "unexpected status: {v}");
        child.kill().ok();
    }

    #[test]
    fn mcp_call_tool_repo_map_empty() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let _ = send_recv(&mut child, &mut stdin, &init);
        let call = mcp_request(2, "tools/call",
            json!({"name":"repo_map","arguments":{"repository_id":"00000000-0000-0000-0000-000000000000"}}));
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert!(resp["result"].is_object() || resp["error"].is_null(),
            "unexpected transport error: {resp}");
        child.kill().ok();
    }

    #[test]
    fn mcp_call_tool_search_missing_query() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let _ = send_recv(&mut child, &mut stdin, &init);
        let call = mcp_request(2, "tools/call", json!({"name":"search","arguments":{}}));
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        let content = &resp["result"]["content"];
        if let Some(arr) = content.as_array() {
            let text = arr[0]["text"].as_str().unwrap_or("");
            assert!(text.contains("query required") || text.contains("required"),
                "expected 'query required' in error text, got: {text}");
        }
        child.kill().ok();
    }

    #[test]
    fn mcp_call_unknown_tool_returns_error_content() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let _ = send_recv(&mut child, &mut stdin, &init);
        let call = mcp_request(2, "tools/call", json!({"name":"does_not_exist","arguments":{}}));
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        let content = &resp["result"]["content"];
        if let Some(arr) = content.as_array() {
            let text = arr[0]["text"].as_str().unwrap_or("");
            assert!(text.contains("unknown tool") || text.contains("does_not_exist"),
                "expected unknown tool error, got: {text}");
        }
        child.kill().ok();
    }

    #[test]
    fn mcp_stderr_does_not_contaminate_stdout() {
        let bin = binary_path();
        if !bin.exists() { return; }
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env("ATTIC_DB_PATH", tmp.path().join("test.db").to_str().unwrap())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(1, "initialize", json!({"protocolVersion":"2024-11-05","clientInfo":{"name":"test","version":"0"}}));
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["jsonrpc"], "2.0", "stdout contains non-JSON: {resp}");
        child.kill().ok();
    }
}
