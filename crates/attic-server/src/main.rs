//! Attic MCP server entry point (Phase 1D).
//!
//! stdout = MCP protocol ONLY. All diagnostics go to tracing (stderr).
//! Absolute filesystem paths are NEVER included in tool responses.
//! All reads use DbPool (concurrent readers); WriterQueueHandle for migrations.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rmcp::{
    ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
        Implementation, ListToolsResult, ServerCapabilities, ServerInfo, Tool,
    },
    service::{RequestContext, RoleServer, serve_server},
    transport::io::stdio,
};
use serde_json::Value;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

use attic_storage::{
    DbPool, FtsSearchParams, MAX_SEARCH_RESULTS,
    fts_path_lookup, fts_search, run_migrations,
};
use attic_storage::connection::open_db;

// ---------------------------------------------------------------------------
// Input validation constants
// ---------------------------------------------------------------------------

const MAX_QUERY_LEN: usize = 1_024;
const MIN_QUERY_LEN: usize = 1;
const MAX_PATH_LEN: usize = 4_096;
const MAX_RESULTS_HARD_CAP: usize = 200;
const DEFAULT_SEARCH_RESULTS: usize = 20;
const DEFAULT_FILE_RESULTS: usize = 50;

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct AtticServer {
    pool: DbPool,
    /// Writer connection kept alive to hold the WAL open; not used directly
    /// after migration.
    #[allow(dead_code)]
    _writer: Arc<Mutex<rusqlite::Connection>>,
}

impl AtticServer {
    fn new(db_path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let (writer_conn, pool) = open_db(db_path)?;
        run_migrations(&writer_conn)?;
        Ok(Self {
            pool,
            _writer: Arc::new(Mutex::new(writer_conn)),
        })
    }
}

// ---------------------------------------------------------------------------
// Tool response helpers
// ---------------------------------------------------------------------------

fn tool_error(msg: impl Into<String>) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(msg.into())]).into()
}

fn tool_ok(text: impl Into<String>) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(text.into())]).into()
}

// ---------------------------------------------------------------------------
// Input validation
// ---------------------------------------------------------------------------

fn validate_query(args: &Value) -> Result<String, CallToolResponse> {
    let q = match args.get("query").and_then(|v| v.as_str()) {
        Some(s) => s.to_owned(),
        None => return Err(tool_error("'query' is required and must be a non-empty string")),
    };
    if q.trim().len() < MIN_QUERY_LEN {
        return Err(tool_error("'query' must not be empty or whitespace-only"));
    }
    if q.len() > MAX_QUERY_LEN {
        return Err(tool_error(format!(
            "'query' exceeds maximum length of {MAX_QUERY_LEN} bytes"
        )));
    }
    Ok(q)
}

fn validate_path(args: &Value) -> Result<String, CallToolResponse> {
    let p = match args.get("path").and_then(|v| v.as_str()) {
        Some(s) => s.to_owned(),
        None => return Err(tool_error("'path' is required and must be a non-empty string")),
    };
    if p.trim().is_empty() {
        return Err(tool_error("'path' must not be empty or whitespace-only"));
    }
    if p.len() > MAX_PATH_LEN {
        return Err(tool_error(format!(
            "'path' exceeds maximum length of {MAX_PATH_LEN} bytes"
        )));
    }
    if p.starts_with('/') || p.starts_with('\\') || p.contains("..") {
        return Err(tool_error(
            "'path' must be a repo-relative path (no leading slash or '..')",
        ));
    }
    Ok(p)
}

fn clamp_max_results(args: &Value, default: usize) -> usize {
    let requested = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(default);
    requested.min(MAX_RESULTS_HARD_CAP).min(MAX_SEARCH_RESULTS)
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

fn handle_search(db: &rusqlite::Connection, args: &Value) -> CallToolResponse {
    let query = match validate_query(args) {
        Ok(q) => q,
        Err(resp) => return resp,
    };

    let repository_id = args.get("repository_id").and_then(|v| v.as_str()).map(str::to_owned);
    let file_type = args.get("file_type").and_then(|v| v.as_str()).map(str::to_owned);
    let language = args.get("language").and_then(|v| v.as_str()).map(str::to_owned);
    let max_results = clamp_max_results(args, DEFAULT_SEARCH_RESULTS);

    let params = FtsSearchParams {
        query: &query,
        repository_id: repository_id.as_deref(),
        file_type: file_type.as_deref(),
        language: language.as_deref(),
        max_results,
    };

    match fts_search(db, &params) {
        Ok(results) if results.is_empty() => tool_ok("No results found."),
        Ok(results) => {
            let mut out = format!("{} result(s):\n\n", results.len());
            for (i, r) in results.iter().enumerate() {
                // Only repo-relative path and display name -- no root_path.
                out.push_str(&format!("--- [{i}] {} ({})\n", r.path, r.repository_name));
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    out.push_str(&format!("Lines {s}-{e}\n"));
                }
                out.push_str(&format!("Score: {:.4}\n", r.score));
                out.push_str(&r.body);
                out.push('\n');
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, query = %query, "fts_search failed");
            tool_error("Search failed -- see server logs for details")
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: file
// ---------------------------------------------------------------------------

fn handle_file(db: &rusqlite::Connection, args: &Value) -> CallToolResponse {
    let path = match validate_path(args) {
        Ok(p) => p,
        Err(resp) => return resp,
    };

    let repository_id = args.get("repository_id").and_then(|v| v.as_str()).map(str::to_owned);
    let max_results = clamp_max_results(args, DEFAULT_FILE_RESULTS);

    match fts_path_lookup(db, &path, repository_id.as_deref(), max_results) {
        Ok(results) if results.is_empty() => {
            tool_ok(format!("No indexed units found for path: {path}"))
        }
        Ok(results) => {
            let mut out = format!("File: {path}\n{} unit(s):\n\n", results.len());
            for (i, r) in results.iter().enumerate() {
                out.push_str(&format!("--- [{i}]"));
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    out.push_str(&format!(" lines {s}-{e}"));
                }
                out.push('\n');
                out.push_str(&r.body);
                out.push('\n');
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, path = %path, "fts_path_lookup failed");
            tool_error("File lookup failed -- see server logs for details")
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: repo_map (root_path omitted intentionally)
// ---------------------------------------------------------------------------

fn handle_repo_map(db: &rusqlite::Connection, _args: &Value) -> CallToolResponse {
    let sql = "
        SELECT r.id, r.display_name,
               COUNT(DISTINCT fo.id) AS files,
               COUNT(ru.id)          AS units
          FROM core_repositories r
          LEFT JOIN core_file_identities  fi ON fi.repository_id = r.id
          LEFT JOIN core_file_occurrences fo ON fo.file_identity_id = fi.id
          LEFT JOIN core_retrieval_units  ru ON ru.file_occurrence_id = fo.id
         GROUP BY r.id
         ORDER BY r.display_name
    ";

    match db.prepare(sql).and_then(|mut stmt| {
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
    }) {
        Ok(repos) if repos.is_empty() => tool_ok("No repositories indexed yet."),
        Ok(repos) => {
            let mut out = format!("{} repository/repositories:\n\n", repos.len());
            for (id, name, files, units) in &repos {
                out.push_str(&format!(
                    "  {name}\n    id: {id}\n    files: {files}, units: {units}\n\n"
                ));
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, "repo_map query failed");
            tool_error("repo_map failed -- see server logs for details")
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: status (db_path omitted intentionally)
// ---------------------------------------------------------------------------

fn handle_status(db: &rusqlite::Connection) -> CallToolResponse {
    let migrations: i64 = db
        .query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |r| r.get(0))
        .unwrap_or(0);
    let repositories: i64 = db
        .query_row("SELECT COUNT(*) FROM core_repositories", [], |r| r.get(0))
        .unwrap_or(0);
    let units: i64 = db
        .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| r.get(0))
        .unwrap_or(0);

    tool_ok(format!(
        "Attic MCP server -- Phase 1D\nstatus:       ok\nmigrations:   {migrations}\nrepositories: {repositories}\nunits:        {units}\n"
    ))
}

// ---------------------------------------------------------------------------
// Schema helper
// ---------------------------------------------------------------------------

fn simple_schema(fields: &[(&str, &str, bool)]) -> Arc<serde_json::Map<String, Value>> {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, typ, req) in fields {
        properties.insert((*name).to_owned(), serde_json::json!({ "type": typ }));
        if *req {
            required.push(Value::String((*name).to_owned()));
        }
    }
    let mut schema = serde_json::Map::new();
    schema.insert("type".to_owned(), Value::String("object".to_owned()));
    if !properties.is_empty() {
        schema.insert("properties".to_owned(), Value::Object(properties));
    }
    if !required.is_empty() {
        schema.insert("required".to_owned(), Value::Array(required));
    }
    Arc::new(schema)
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

impl ServerHandler for AtticServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        let mut capabilities = ServerCapabilities::default();
        capabilities.tools = Some(rmcp::model::ToolsCapability::default());
        info.capabilities = capabilities;
        let mut server_impl = Implementation::from_build_env();
        server_impl.name = "attic".to_owned();
        server_impl.version = env!("CARGO_PKG_VERSION").to_owned();
        info.server_info = server_impl;
        info.instructions = Some(
            "Attic: code-search MCP server. Tools: search, file, repo_map, status.".to_owned(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
        Ok(ListToolsResult {
            tools: vec![
                Tool::new(
                    "search",
                    "Full-text search over indexed code. Supports FTS5 syntax. Query 1-1024 bytes; max_results capped at 200.",
                    simple_schema(&[
                        ("query", "string", true),
                        ("repository_id", "string", false),
                        ("file_type", "string", false),
                        ("language", "string", false),
                        ("max_results", "number", false),
                    ]),
                ),
                Tool::new(
                    "file",
                    "Return indexed retrieval units for a repo-relative file path. No '..' or leading slashes.",
                    simple_schema(&[
                        ("path", "string", true),
                        ("repository_id", "string", false),
                        ("max_results", "number", false),
                    ]),
                ),
                Tool::new(
                    "repo_map",
                    "List all indexed repositories with file and unit counts.",
                    simple_schema(&[]),
                ),
                Tool::new(
                    "status",
                    "Return server health and database statistics.",
                    simple_schema(&[]),
                ),
            ],
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
        let args: Value = request.arguments.map(Value::Object).unwrap_or(Value::Null);
        let tool_name = request.name.clone();

        let known = matches!(tool_name.as_ref(), "search" | "file" | "repo_map" | "status");
        if !known {
            warn!(tool = %tool_name, "unknown tool called");
            return Err(rmcp::model::ErrorData {
                code: rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                message: format!("unknown tool: {tool_name}").into(),
                data: None,
            });
        }

        let resp = self.pool.with_reader(|conn| {
            Ok(match tool_name.as_ref() {
                "search"   => handle_search(conn, &args),
                "file"     => handle_file(conn, &args),
                "repo_map" => handle_repo_map(conn, &args),
                "status"   => handle_status(conn),
                _          => unreachable!("checked above"),
            })
        });

        match resp {
            Ok(r) => Ok(r),
            Err(e) => {
                error!(error = %e, tool = %tool_name, "pool reader error");
                Ok(tool_error("Internal error -- see server logs for details"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let db_path = std::env::var("ATTIC_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs_or_home().join(".mcp").join("attic.db"));

    // Path only ever reaches stderr via tracing -- never stdout/MCP frames.
    info!(db_path = %db_path.display(), "attic MCP server starting");

    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(%e, "failed to create db directory");
            std::process::exit(1);
        }
    }

    let server = match AtticServer::new(&db_path) {
        Ok(s) => s,
        Err(e) => {
            error!(%e, "failed to initialise Attic server");
            std::process::exit(1);
        }
    };

    info!("attic MCP server ready -- listening on stdio");

    let transport = stdio();
    if let Err(e) = serve_server(server, transport).await {
        error!(%e, "MCP server exited with error");
        std::process::exit(1);
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_server() -> (AtticServer, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let server = AtticServer::new(&db_path).unwrap();
        (server, tmp)
    }

    fn with_reader<F, R>(server: &AtticServer, f: F) -> R
    where
        F: FnOnce(&rusqlite::Connection) -> R,
    {
        server.pool.with_reader(|conn| Ok(f(conn))).unwrap()
    }

    fn extract_text(resp: &CallToolResponse) -> String {
        match resp {
            CallToolResponse::Complete(r) => match r.content.first() {
                Some(ContentBlock::Text(t)) => t.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    }

    fn is_error(resp: &CallToolResponse) -> bool {
        match resp {
            CallToolResponse::Complete(r) => r.is_error.unwrap_or(false),
            _ => false,
        }
    }

    // status

    #[test]
    fn status_tool_returns_ok() {
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, handle_status);
        assert!(!is_error(&resp), "status should not be an error");
        let text = extract_text(&resp);
        assert!(text.contains("Phase 1D"), "should mention phase");
    }

    #[test]
    fn status_does_not_expose_db_path() {
        let (server, tmp) = make_server();
        let resp = with_reader(&server, handle_status);
        let text = extract_text(&resp);
        let abs = tmp.path().to_str().unwrap();
        assert!(
            !text.contains(abs),
            "status must not expose db_path; text={text:?}"
        );
    }

    // repo_map

    #[test]
    fn repo_map_tool_empty_db() {
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, |conn| handle_repo_map(conn, &Value::Null));
        assert!(!is_error(&resp));
        let text = extract_text(&resp);
        assert!(text.contains("No repositories"), "empty db should say no repos");
    }

    #[test]
    fn repo_map_does_not_expose_root_path() {
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, |conn| handle_repo_map(conn, &Value::Null));
        let text = extract_text(&resp);
        assert!(!text.contains("root:"), "repo_map must not expose root_path");
    }

    // search -- validation

    #[test]
    fn search_tool_requires_query() {
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, |conn| handle_search(conn, &Value::Null));
        assert!(is_error(&resp), "missing query should be error");
    }

    #[test]
    fn search_rejects_empty_query() {
        let (server, _tmp) = make_server();
        let args = serde_json::json!({ "query": "   " });
        let resp = with_reader(&server, |conn| handle_search(conn, &args));
        assert!(is_error(&resp), "whitespace-only query should be error");
    }

    #[test]
    fn search_rejects_overlong_query() {
        let (server, _tmp) = make_server();
        let long_q = "a".repeat(MAX_QUERY_LEN + 1);
        let args = serde_json::json!({ "query": long_q });
        let resp = with_reader(&server, |conn| handle_search(conn, &args));
        assert!(is_error(&resp), "overlong query should be error");
    }

    #[test]
    fn search_tool_empty_results() {
        let (server, _tmp) = make_server();
        let args = serde_json::json!({ "query": "nonexistent_term_xyz_12345" });
        let resp = with_reader(&server, |conn| handle_search(conn, &args));
        assert!(!is_error(&resp), "no results is not an error");
    }

    #[test]
    fn search_caps_max_results() {
        let args = serde_json::json!({ "query": "x", "max_results": 9999 });
        let capped = clamp_max_results(&args, DEFAULT_SEARCH_RESULTS);
        assert!(
            capped <= MAX_RESULTS_HARD_CAP,
            "max_results must be capped at {MAX_RESULTS_HARD_CAP}"
        );
    }

    // file -- validation

    #[test]
    fn file_tool_requires_path() {
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, |conn| handle_file(conn, &Value::Null));
        assert!(is_error(&resp), "missing path should be error");
    }

    #[test]
    fn file_rejects_absolute_path() {
        let (server, _tmp) = make_server();
        let args = serde_json::json!({ "path": "/etc/passwd" });
        let resp = with_reader(&server, |conn| handle_file(conn, &args));
        assert!(is_error(&resp), "absolute path should be error");
    }

    #[test]
    fn file_rejects_path_traversal() {
        let (server, _tmp) = make_server();
        let args = serde_json::json!({ "path": "../../secret" });
        let resp = with_reader(&server, |conn| handle_file(conn, &args));
        assert!(is_error(&resp), "path traversal should be error");
    }

    #[test]
    fn file_rejects_overlong_path() {
        let (server, _tmp) = make_server();
        let long_p = "a".repeat(MAX_PATH_LEN + 1);
        let args = serde_json::json!({ "path": long_p });
        let resp = with_reader(&server, |conn| handle_file(conn, &args));
        assert!(is_error(&resp), "overlong path should be error");
    }

    // schema helper

    #[test]
    fn simple_schema_required_field() {
        let schema = simple_schema(&[("query", "string", true)]);
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str().unwrap(), "query");
    }

    #[test]
    fn simple_schema_optional_field_not_in_required() {
        let schema = simple_schema(&[
            ("query", "string", true),
            ("max_results", "number", false),
        ]);
        let required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("required array");
        assert_eq!(required.len(), 1);
        let names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            !names.contains(&"max_results"),
            "optional field must not be in required"
        );
    }
}
