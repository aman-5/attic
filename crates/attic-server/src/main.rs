//! Attic MCP server entry point (Phase 1D).
//!
//! # Critical constraints
//!
//! - **stdout = MCP protocol ONLY**.  Every `println!` / `print!` that reaches
//!   stdout will corrupt the JSON-RPC framing.  All diagnostics go to
//!   `tracing` (which is routed to **stderr**).
//! - This binary is spawned by an MCP host via stdio transport.

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

use attic_storage::{FtsSearchParams, MAX_SEARCH_RESULTS, fts_path_lookup, fts_search,
    run_migrations};

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

/// Shared state for all MCP tool handlers.
///
/// The `rusqlite::Connection` is not `Send`, so we guard it with a `Mutex`.
/// All tool calls are serialised through this lock — acceptable for Phase 1D
/// (single-writer, low-throughput developer tool).
struct AtticServer {
    db: Arc<Mutex<rusqlite::Connection>>,
    db_path: PathBuf,
}

impl AtticServer {
    fn new(db_path: PathBuf) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = attic_storage::connection::open_rw(&db_path)?;
        run_migrations(&conn)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
            db_path,
        })
    }
}

// ---------------------------------------------------------------------------
// Helper: convert tool errors to Ok(CallToolResult)
// ---------------------------------------------------------------------------

fn tool_error(msg: impl Into<String>) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(msg.into())]).into()
}

fn tool_ok(text: impl Into<String>) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(text.into())]).into()
}

// ---------------------------------------------------------------------------
// Tool: search
// ---------------------------------------------------------------------------

/// `search` — Full-text search over retrieval units.
///
/// Arguments (all optional except `query`):
/// - `query`: FTS5 query string (required)
/// - `repository_id`: scope to a specific repository UUID
/// - `file_type`: filter by file type string (e.g. "Rust")
/// - `language`: filter by language string
/// - `max_results`: maximum number of results (default 20, max 200)
fn handle_search(db: &rusqlite::Connection, args: &Value) -> CallToolResponse {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) if !q.is_empty() => q.to_owned(),
        _ => return tool_error("'query' argument is required and must be a non-empty string"),
    };

    let repository_id = args
        .get("repository_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let file_type = args
        .get("file_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let language = args
        .get("language")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;
    let max_results = max_results.min(MAX_SEARCH_RESULTS);

    let params = FtsSearchParams {
        query: &query,
        repository_id: repository_id.as_deref(),
        file_type: file_type.as_deref(),
        language: language.as_deref(),
        max_results,
    };

    match fts_search(db, &params) {
        Ok(results) if results.is_empty() => {
            tool_ok("No results found.")
        }
        Ok(results) => {
            let mut out = String::new();
            out.push_str(&format!("{} result(s):\n\n", results.len()));
            for (i, r) in results.iter().enumerate() {
                out.push_str(&format!(
                    "--- [{i}] {} ({})\n",
                    r.path,
                    &r.repository_name,
                ));
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    out.push_str(&format!("Lines {s}–{e}\n"));
                }
                out.push_str(&format!("Score: {:.4}\n", r.score));
                out.push_str(&r.body);
                out.push('\n');
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, "fts_search failed");
            tool_error(format!("Search failed: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: file
// ---------------------------------------------------------------------------

/// `file` — Look up retrieval units for a specific file path.
///
/// Arguments:
/// - `path`: repo-relative path (required)
/// - `repository_id`: scope to a specific repository UUID (optional)
/// - `max_results`: maximum results (default 50)
fn handle_file(db: &rusqlite::Connection, args: &Value) -> CallToolResponse {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => return tool_error("'path' argument is required and must be a non-empty string"),
    };

    let repository_id = args
        .get("repository_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as usize;

    match fts_path_lookup(db, &path, repository_id.as_deref(), max_results) {
        Ok(results) if results.is_empty() => {
            tool_ok(format!("No indexed units found for path: {path}"))
        }
        Ok(results) => {
            let mut out = String::new();
            out.push_str(&format!("File: {path}\n{} unit(s):\n\n", results.len()));
            for (i, r) in results.iter().enumerate() {
                out.push_str(&format!("--- [{i}]"));
                if let (Some(s), Some(e)) = (r.start_line, r.end_line) {
                    out.push_str(&format!(" lines {s}–{e}"));
                }
                out.push('\n');
                out.push_str(&r.body);
                out.push('\n');
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, "fts_path_lookup failed");
            tool_error(format!("File lookup failed: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: repo_map
// ---------------------------------------------------------------------------

/// `repo_map` — List indexed repositories and their statistics.
fn handle_repo_map(db: &rusqlite::Connection, _args: &Value) -> CallToolResponse {
    let sql = "
        SELECT r.id, r.display_name, r.root_path,
               COUNT(DISTINCT fo.id)  AS files,
               COUNT(ru.id)           AS units
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
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect::<Vec<_>>())
    }) {
        Ok(repos) if repos.is_empty() => tool_ok("No repositories indexed yet."),
        Ok(repos) => {
            let mut out = format!("{} repository/repositories:\n\n", repos.len());
            for (id, name, root, files, units) in &repos {
                out.push_str(&format!(
                    "  {name}\n    id: {id}\n    root: {root}\n    files: {files}, units: {units}\n\n"
                ));
            }
            tool_ok(out)
        }
        Err(e) => {
            error!(error = %e, "repo_map query failed");
            tool_error(format!("repo_map failed: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// Tool: status
// ---------------------------------------------------------------------------

/// `status` — Return server health and database statistics.
fn handle_status(db: &rusqlite::Connection, db_path: &std::path::Path) -> CallToolResponse {
    let migrations: i64 = db
        .query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |r| r.get(0))
        .unwrap_or(0);
    let repositories: i64 = db
        .query_row("SELECT COUNT(*) FROM core_repositories", [], |r| r.get(0))
        .unwrap_or(0);
    let units: i64 = db
        .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| r.get(0))
        .unwrap_or(0);

    let out = format!(
        "Attic MCP server — Phase 1D\n\
         db_path:      {}\n\
         migrations:   {migrations}\n\
         repositories: {repositories}\n\
         units:        {units}\n",
        db_path.display(),
    );
    tool_ok(out)
}

// ---------------------------------------------------------------------------
// Schema helper
// ---------------------------------------------------------------------------

/// Build a minimal JSON Schema `Arc<JsonObject>` for tool input parameters.
fn simple_schema(fields: &[(&str, &str, bool)]) -> Arc<serde_json::Map<String, Value>> {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();

    for (name, typ, req) in fields {
        properties.insert(
            (*name).to_owned(),
            serde_json::json!({ "type": typ }),
        );
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
// ServerHandler implementation
// ---------------------------------------------------------------------------

impl ServerHandler for AtticServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo, ServerCapabilities, Implementation and ToolsCapability are all
        // #[non_exhaustive] in rmcp 3.1.4 — must use Default + field mutation.
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
                    "Full-text search over indexed code retrieval units. \
                     Supports FTS5 query syntax (prefix*, phrase \"...\", boolean AND/OR/NOT).",
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
                    "Return all indexed retrieval units for a given file path.",
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
                    "Return server health information and database statistics.",
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
        let args: Value = request
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Null);

        let db = match self.db.lock() {
            Ok(guard) => guard,
            Err(e) => {
                error!("db lock poisoned: {e}");
                return Ok(tool_error("Internal error: database lock poisoned"));
            }
        };

        let response = match request.name.as_ref() {
            "search"   => handle_search(&db, &args),
            "file"     => handle_file(&db, &args),
            "repo_map" => handle_repo_map(&db, &args),
            "status"   => handle_status(&db, &self.db_path),
            unknown => {
                warn!(tool = %unknown, "unknown tool called");
                return Err(rmcp::model::ErrorData {
                    code: rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                    message: format!("unknown tool: {unknown}").into(),
                    data: None,
                });
            }
        };

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Route all tracing to stderr — stdout is reserved for MCP protocol frames.
    fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Resolve database path: $ATTIC_DB_PATH or default.
    let db_path = std::env::var("ATTIC_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_or_home().join(".mcp").join("attic.db")
        });

    info!(db_path = %db_path.display(), "attic MCP server starting");

    // Ensure parent directory exists.
    if let Some(parent) = db_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!(%e, "failed to create db directory");
            std::process::exit(1);
        }
    }

    // Initialise the server state.
    let server = match AtticServer::new(db_path) {
        Ok(s) => s,
        Err(e) => {
            error!(%e, "failed to initialise Attic server");
            std::process::exit(1);
        }
    };

    info!("attic MCP server ready — listening on stdio");

    // Serve until the client disconnects.
    // serve_server(service, transport) — service is the first argument.
    let transport = stdio();
    if let Err(e) = serve_server(server, transport).await {
        error!(%e, "MCP server exited with error");
        std::process::exit(1);
    }
}

/// Return a sensible default base directory.
fn dirs_or_home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server() -> (AtticServer, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let server = AtticServer::new(db_path).unwrap();
        (server, tmp)
    }

    #[test]
    fn status_tool_returns_ok() {
        let (server, _tmp) = make_server();
        let db = server.db.lock().unwrap();
        let resp = handle_status(&db, &server.db_path);
        match resp {
            CallToolResponse::Complete(r) => {
                assert!(!r.is_error.unwrap_or(false), "status should not be an error");
                let block = r.content.first().expect("should have content");
                let text = match block {
                    ContentBlock::Text(t) => t.text.as_str(),
                    _ => panic!("expected text block"),
                };
                assert!(text.contains("Phase 1D"), "should mention phase");
            }
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn repo_map_tool_empty_db() {
        let (server, _tmp) = make_server();
        let db = server.db.lock().unwrap();
        let resp = handle_repo_map(&db, &Value::Null);
        match resp {
            CallToolResponse::Complete(r) => {
                assert!(!r.is_error.unwrap_or(false));
                let block = r.content.first().expect("should have content");
                let text = match block {
                    ContentBlock::Text(t) => t.text.as_str(),
                    _ => panic!("expected text block"),
                };
                assert!(text.contains("No repositories"), "empty db should say no repos");
            }
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn search_tool_requires_query() {
        let (server, _tmp) = make_server();
        let db = server.db.lock().unwrap();
        let resp = handle_search(&db, &Value::Null);
        match resp {
            CallToolResponse::Complete(r) => {
                assert!(r.is_error.unwrap_or(false), "missing query should be error");
            }
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn search_tool_empty_results() {
        let (server, _tmp) = make_server();
        let db = server.db.lock().unwrap();
        let args = serde_json::json!({ "query": "nonexistent_term_xyz_12345" });
        let resp = handle_search(&db, &args);
        match resp {
            CallToolResponse::Complete(r) => {
                assert!(!r.is_error.unwrap_or(false), "no results is not an error");
            }
            _ => panic!("unexpected response variant"),
        }
    }

    #[test]
    fn simple_schema_required_field() {
        let schema = simple_schema(&[("query", "string", true)]);
        let required = schema.get("required").and_then(|v| v.as_array()).expect("required array");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str().unwrap(), "query");
    }

    #[test]
    fn file_tool_requires_path() {
        let (server, _tmp) = make_server();
        let db = server.db.lock().unwrap();
        let resp = handle_file(&db, &Value::Null);
        match resp {
            CallToolResponse::Complete(r) => {
                assert!(r.is_error.unwrap_or(false), "missing path should be error");
            }
            _ => panic!("unexpected response variant"),
        }
    }
}
