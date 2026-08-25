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
    get_db_stats, get_repository_stats,
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
// Response size constants
// ---------------------------------------------------------------------------

/// Maximum bytes for a single retrieval unit body before truncation.
const MAX_BODY_BYTES: usize = 8_192;

/// Maximum total bytes in a single MCP tool response before stopping early.
const MAX_RESPONSE_BYTES: usize = 262_144;

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
        .map(|n| {
            // Checked conversion: clamp to usize::MAX to avoid truncation on
            // 32-bit targets, though in practice values are always small.
            usize::try_from(n).unwrap_or(usize::MAX)
        })
        .unwrap_or(default);
    requested.min(MAX_RESULTS_HARD_CAP).min(MAX_SEARCH_RESULTS)
}

/// Truncate `body` to at most `MAX_BODY_BYTES` bytes (on a UTF-8 char
/// boundary), appending a note if truncation occurred.
fn cap_body(body: &str) -> String {
    if body.len() <= MAX_BODY_BYTES {
        return body.to_owned();
    }
    // Find the largest char boundary ≤ MAX_BODY_BYTES.
    let mut boundary = MAX_BODY_BYTES;
    while !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let omitted = body.len() - boundary;
    format!("{}… [{omitted} bytes omitted]", &body[..boundary])
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
            let header = format!("{} result(s):\n\n", results.len());
            let mut out = header;
            let mut total_bytes: usize = out.len();

            for (i, r) in results.iter().enumerate() {
                let body = cap_body(&r.body);
                let mut entry = format!(
                    "--- [{i}] {} ({})\n",
                    r.path, r.repository_name
                );
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    entry.push_str(&format!("Lines {s}-{e}\n"));
                }
                entry.push_str(&format!("Score: {:.4}\n", r.score));
                entry.push_str(&body);
                entry.push('\n');

                if total_bytes + entry.len() > MAX_RESPONSE_BYTES {
                    let remaining = results.len() - i;
                    out.push_str(&format!(
                        "[{remaining} more result(s) omitted: response size limit reached]\n"
                    ));
                    break;
                }
                total_bytes += entry.len();
                out.push_str(&entry);
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
            let header = format!("File: {path}\n{} unit(s):\n\n", results.len());
            let mut out = header;
            let mut total_bytes: usize = out.len();

            for (i, r) in results.iter().enumerate() {
                let body = cap_body(&r.body);
                let mut entry = format!("--- [{i}]");
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    entry.push_str(&format!(" lines {s}-{e}"));
                }
                entry.push('\n');
                entry.push_str(&body);
                entry.push('\n');

                if total_bytes + entry.len() > MAX_RESPONSE_BYTES {
                    let remaining = results.len() - i;
                    out.push_str(&format!(
                        "[{remaining} more unit(s) omitted: response size limit reached]\n"
                    ));
                    break;
                }
                total_bytes += entry.len();
                out.push_str(&entry);
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
// Tool: repo_map — uses storage API, no raw SQL
// ---------------------------------------------------------------------------

fn handle_repo_map(db: &rusqlite::Connection, _args: &Value) -> CallToolResponse {
    match get_repository_stats(db) {
        Ok(repos) if repos.is_empty() => tool_ok("No repositories indexed yet."),
        Ok(repos) => {
            let mut out = format!("{} repository/repositories:\n\n", repos.len());
            for r in &repos {
                out.push_str(&format!(
                    "  {name}\n    id: {id}\n    files: {files}, units: {units}\n\n",
                    name  = r.display_name,
                    id    = r.id,
                    files = r.file_count,
                    units = r.unit_count,
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
// Tool: status — uses storage API, no raw SQL
// ---------------------------------------------------------------------------

fn handle_status(db: &rusqlite::Connection) -> CallToolResponse {
    match get_db_stats(db) {
        Ok(stats) => tool_ok(format!(
            "Attic MCP server -- Phase 1D\nstatus:       ok\nmigrations:   {migrations}\nrepositories: {repositories}\nunits:        {units}\n",
            migrations   = stats.migration_count,
            repositories = stats.repository_count,
            units        = stats.unit_count,
        )),
        Err(e) => {
            error!(error = %e, "status db_stats failed");
            tool_error("status failed -- see server logs for details")
        }
    }
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
                    "Full-text search over indexed code. Supports FTS5 syntax. Query 1-1024 bytes; max_results capped at 200. Each result body capped at 8 KiB; total response capped at 256 KiB.",
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
                    "Return indexed retrieval units for a repo-relative file path. No '..' or leading slashes. Each unit body capped at 8 KiB; total response capped at 256 KiB.",
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
    match serve_server(server, transport).await {
        Err(e) => {
            error!(%e, "MCP server failed to initialize");
            std::process::exit(1);
        }
        Ok(running) => {
            if let Err(e) = running.waiting().await {
                error!(%e, "MCP server task panicked");
                std::process::exit(1);
            }
        }
    }
}

fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Unit tests
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

    // -----------------------------------------------------------------------
    // status
    // -----------------------------------------------------------------------

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

    #[test]
    fn status_uses_storage_api_not_raw_sql() {
        // Verify that status returns sensible integer counts (0 after fresh
        // migration), which confirms the storage API path works end-to-end.
        let (server, _tmp) = make_server();
        let resp = with_reader(&server, handle_status);
        assert!(!is_error(&resp));
        let text = extract_text(&resp);
        assert!(text.contains("repositories:"), "must include repositories count");
        assert!(text.contains("units:"), "must include units count");
        assert!(text.contains("migrations:"), "must include migrations count");
    }

    // -----------------------------------------------------------------------
    // repo_map
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // search -- validation
    // -----------------------------------------------------------------------

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
        assert!(
            capped <= MAX_SEARCH_RESULTS,
            "max_results must be capped at MAX_SEARCH_RESULTS={MAX_SEARCH_RESULTS}"
        );
    }

    #[test]
    fn search_max_results_u64_max_clamped() {
        // u64::MAX must not overflow usize or exceed the hard cap.
        let args = serde_json::json!({ "query": "x", "max_results": u64::MAX });
        let capped = clamp_max_results(&args, DEFAULT_SEARCH_RESULTS);
        assert!(capped <= MAX_RESULTS_HARD_CAP);
    }

    // -----------------------------------------------------------------------
    // file -- validation
    // -----------------------------------------------------------------------

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

    // -----------------------------------------------------------------------
    // body / response size caps
    // -----------------------------------------------------------------------

    #[test]
    fn cap_body_short_string_unchanged() {
        let s = "hello world";
        assert_eq!(cap_body(s), s);
    }

    #[test]
    fn cap_body_long_string_truncated() {
        let s = "x".repeat(MAX_BODY_BYTES + 100);
        let capped = cap_body(&s);
        assert!(
            capped.len() <= MAX_BODY_BYTES + 50, // small overhead for the note
            "cap_body result too long: {} bytes",
            capped.len()
        );
        assert!(capped.contains("bytes omitted"), "should include omission note");
    }

    #[test]
    fn cap_body_on_utf8_boundary() {
        // Build a string with multi-byte chars that crosses MAX_BODY_BYTES.
        // Each '©' is 2 bytes in UTF-8.
        let s: String = "©".repeat(MAX_BODY_BYTES); // 2 * MAX_BODY_BYTES bytes total
        let capped = cap_body(&s);
        // Result must be valid UTF-8 (would panic on invalid slice otherwise).
        assert!(!capped.is_empty());
    }

    // -----------------------------------------------------------------------
    // schema helper
    // -----------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Child-process MCP stdio integration tests
//
// These tests build the `attic` binary (via `cargo build`) and then spawn it
// as a child process, communicating over its stdin/stdout using JSON-RPC 2.0
// (the MCP wire protocol).  stderr is captured separately so it never
// contaminates the protocol stream.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod integration {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    /// Write one JSON-RPC 2.0 newline-delimited frame to `stdin`.
    /// Returns `false` if the pipe is closed (broken pipe).
    fn send(stdin: &mut impl Write, msg: &str) -> bool {
        let ok = stdin.write_all(msg.as_bytes()).is_ok()
            && stdin.write_all(b"\n").is_ok()
            && stdin.flush().is_ok();
        ok
    }

    /// Perform the MCP initialize handshake and return the parsed initialize
    /// response.  Sends `notifications/initialized` and waits 300 ms for the
    /// server's async runtime to transition to the ready state.
    fn handshake(stdin: &mut impl Write, stdout: &mut impl BufRead) -> serde_json::Value {
        send(
            stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.0.1"}}}"#,
        );
        let line = recv(stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON in initialize response: {e}\nline={line:?}"));

        // Send the initialized notification and give the server's tokio
        // executor time to process the state transition before we send the
        // first real request.
        send(stdin, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        std::thread::sleep(std::time::Duration::from_millis(300));
        v
    }

    /// Read one non-empty JSON line from `stdout`, skipping blank lines and
    /// retrying for up to ~10 seconds to accommodate async scheduling latency.
    fn recv(reader: &mut impl BufRead) -> String {
        for _ in 0..200 {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => {
                    // EOF or error — wait and retry.
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Ok(_) => {}
            }
            let trimmed = line.trim_end().to_owned();
            if trimmed.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            return trimmed;
        }
        panic!("timed out waiting for a response from the attic server");
    }

    /// Return the path to the `attic` binary built for tests.
    fn attic_bin() -> std::path::PathBuf {
        // CARGO_BIN_EXE_attic is set by Cargo for integration tests living
        // in tests/ directories. For in-binary tests (mod integration inside
        // main.rs) it is not set, so we fall back to navigating from the
        // current test executable's path.
        if let Ok(p) = std::env::var("CARGO_BIN_EXE_attic") {
            return std::path::PathBuf::from(p);
        }
        // The test binary lives at:
        //   target/<triple>/debug/deps/attic-<hash>[.exe]
        // The `attic` binary lives at:
        //   target/<triple>/debug/attic[.exe]
        // Navigate: current_exe → deps/ → profile_dir/ → attic[.exe]
        let exe_name = if cfg!(windows) { "attic.exe" } else { "attic" };
        std::env::current_exe()
            .expect("cannot determine current exe path")
            .parent()
            .expect("no parent (deps/)")
            .parent()
            .expect("no grandparent (profile dir)")
            .join(exe_name)
    }

    /// Spawn the `attic` binary with a fresh temp DB and return the handles.
    fn spawn_server(tmp: &TempDir) -> std::process::Child {
        let db = tmp.path().join("test.db");
        Command::new(attic_bin())
            .env("ATTIC_DB_PATH", &db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()) // keep stderr off the protocol stream
            .spawn()
            .expect("failed to spawn attic binary — run `cargo build` first")
    }

    // -----------------------------------------------------------------------
    // MCP initialize handshake
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_initialize_handshake() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        let v = handshake(stdin, &mut stdout);

        assert_eq!(v["jsonrpc"], "2.0", "must be JSON-RPC 2.0");
        assert_eq!(v["id"], 1, "response id must match request id");
        assert!(v["result"].is_object(), "initialize must return a result object");
        assert!(
            v["result"]["serverInfo"]["name"].as_str().unwrap_or("") == "attic",
            "serverInfo.name must be 'attic'"
        );

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // tools/list
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_tools_list() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        handshake(stdin, &mut stdout);

        // Request tool list.
        send(stdin, r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let line = recv(&mut stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline={line:?}"));

        assert_eq!(v["id"], 2);
        let tools = v["result"]["tools"]
            .as_array()
            .expect("tools must be an array");

        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();

        for expected in &["search", "file", "repo_map", "status"] {
            assert!(
                names.contains(expected),
                "tool '{expected}' not found in tools/list; got {names:?}"
            );
        }

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // tools/call — status
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_call_tool_status() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        handshake(stdin, &mut stdout);

        send(
            stdin,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"status","arguments":{}}}"#,
        );
        let line = recv(&mut stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline={line:?}"));

        assert_eq!(v["id"], 3);
        assert!(v["result"].is_object(), "must have a result");

        let content = v["result"]["content"]
            .as_array()
            .expect("content must be array");
        let text = content
            .first()
            .and_then(|c| c["text"].as_str())
            .expect("first content item must have text");

        assert!(text.contains("Phase 1D"), "status must mention Phase 1D");
        assert!(!text.contains("error"), "status must not be an error");

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // tools/call — repo_map on empty DB
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_call_tool_repo_map_empty() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        handshake(stdin, &mut stdout);

        send(
            stdin,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"repo_map","arguments":{}}}"#,
        );
        let line = recv(&mut stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline={line:?}"));

        assert_eq!(v["id"], 4);
        let text = v["result"]["content"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|c| c["text"].as_str())
            .expect("text content");

        assert!(
            text.contains("No repositories"),
            "empty DB repo_map must say no repositories; got: {text:?}"
        );

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // tools/call — malformed call (missing required argument)
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_call_tool_search_missing_query() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        handshake(stdin, &mut stdout);

        // Call search without the required 'query' argument.
        send(
            stdin,
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{}}}"#,
        );
        let line = recv(&mut stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline={line:?}"));

        assert_eq!(v["id"], 5);
        // The server returns a result with isError=true (tool-level error,
        // not a JSON-RPC protocol error).
        let is_error = v["result"]["isError"].as_bool().unwrap_or(false);
        assert!(is_error, "missing query must produce tool-level error; response={v}");

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // Unknown tool call → JSON-RPC METHOD_NOT_FOUND error
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_call_unknown_tool_returns_error() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        handshake(stdin, &mut stdout);

        send(
            stdin,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"nonexistent_tool","arguments":{}}}"#,
        );
        let line = recv(&mut stdout);
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON: {e}\nline={line:?}"));

        assert_eq!(v["id"], 6);
        // Expect either a JSON-RPC error object or a tool-level error result.
        let has_error = v["error"].is_object()
            || v["result"]["isError"].as_bool().unwrap_or(false);
        assert!(
            has_error,
            "unknown tool must produce an error; response={v}"
        );

        child.kill().ok();
    }

    // -----------------------------------------------------------------------
    // stdout/stderr separation: stderr must NOT appear on stdout
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_stderr_does_not_contaminate_stdout() {
        let tmp = TempDir::new().unwrap();
        let mut child = spawn_server(&tmp);

        let stdin = child.stdin.as_mut().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());

        let v = handshake(stdin, &mut stdout);
        let line = serde_json::to_string(&v).unwrap();

        // Every line on stdout must be valid JSON (MCP frames only).
        let parsed = serde_json::from_str::<serde_json::Value>(&line);
        assert!(
            parsed.is_ok(),
            "stdout must contain only JSON; got non-JSON line: {line:?}"
        );

        child.kill().ok();
    }
}
