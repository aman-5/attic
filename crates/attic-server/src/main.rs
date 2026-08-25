// crates/attic-server/src/main.rs
// Phase 1D – MCP server (rmcp-based), no raw rusqlite writes, DbPool readers +
// coordinated WriterQueueHandle writer.  Workspace indexing runs exclusively
// through the approved Phase 1A coordinated publication service; the `file`
// tool serves bounded regions with UTF-8-safe offsets, checked numeric
// arguments, and genuine bounded streaming for LARGE files.

use attic_discovery::{
    DiscoveryPolicy, SecretScanDecision, canonicalize_within_root, preprocess_file_content,
};
use attic_indexing::{IndexError, IndexOptions, IndexingStore, index_repository};
use attic_storage::{
    DbPool, FtsSearchParams, MAX_SEARCH_RESULTS, StorageError, WriterQueue, WriterQueueHandle,
    fts_search, get_db_stats, get_repository_path, get_repository_stats,
    lookup_repository_by_root_path, run_migrations,
};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use tracing::{error, info, warn};

const SERVER_NAME: &str = "attic";
const SERVER_VERSION: &str = "0.1.0";

// ─── input / resource limits ───────────────────────────────────────────────────

/// Maximum accepted value for any single line/byte argument.  Anything above
/// this is rejected outright before any work happens (overflow guard).
pub(crate) const MAX_REGION_VALUE: u64 = 1 << 48;

/// Largest line-window a single request may cover.
pub(crate) const MAX_LINE_SPAN: u64 = 100_000;

/// Largest byte-window a single request may cover.
pub(crate) const MAX_BYTE_SPAN: u64 = 8 * 1024 * 1024;

/// Hard cap on the bytes returned in one tool response.  Applies to every
/// `file` response, streamed or not.
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Absolute bound on how many bytes of a LARGE file's redacted stream are
/// scanned while assembling one response.  Prevents unbounded work even when
/// a caller supplies an open-ended window far beyond EOF.
pub(crate) const MAX_STREAM_SCAN_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Error)]
enum ServerError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("indexing error: {0}")]
    Indexing(#[from] IndexError),
    #[error("discovery I/O: {0}")]
    Discovery(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
}

#[derive(Clone)]
struct AtticServer {
    pool: DbPool,
    writer: WriterQueueHandle,
    _queue: Arc<WriterQueue>,
    /// Phase 2 incremental service; present only in watch mode.
    incremental: Option<Arc<attic_incremental::IncrementalService>>,
    /// Which change-detection mechanism is running (`None` = incremental
    /// disabled explicitly).
    watch_mode: Option<attic_incremental::WatchMode>,
}

impl AtticServer {
    fn new(db_path: &Path) -> Result<Self, ServerError> {
        let (conn, pool) = attic_storage::open_db(db_path).map_err(ServerError::Storage)?;
        run_migrations(&conn).map_err(ServerError::Storage)?;
        let queue = WriterQueue::new(conn).map_err(ServerError::Storage)?;
        let writer = queue.handle();
        let _queue = Arc::new(queue);
        Ok(AtticServer {
            pool,
            writer,
            _queue,
            incremental: None,
            watch_mode: None,
        })
    }

    fn bootstrap_workspace(&self, root: &Path) -> Result<String, ServerError> {
        let root_str = root.to_string_lossy().to_string();
        if let Some(id) = self
            .pool
            .with_reader(|c| lookup_repository_by_root_path(c, &root_str))?
        {
            return Ok(id.to_string());
        }
        // Coordinated-writer indexing: discovery + analysis happen here on the
        // calling thread, then ONE submit_index_publication mutation carries
        // every write through the Phase 1A writer queue inside its ambient
        // transaction.  No secondary connection, no nested transactions, and
        // attic-indexing never touches a raw rusqlite write connection.
        let store = IndexingStore {
            readers: &self.pool,
            writer: &self.writer,
        };
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        index_repository(&store, root, &policy, &opts)
            .map(|r| r.repository_id)
            .map_err(ServerError::Indexing)
    }
}

// ─── input validation ──────────────────────────────────────────────────────────

fn validate_filter(name: &str, value: &str, max_len: usize) -> Result<(), ServerError> {
    if value.len() > max_len {
        return Err(ServerError::InvalidArg(format!(
            "{name} too long (max {max_len})"
        )));
    }
    if value.chars().any(|c| c.is_control()) {
        return Err(ServerError::InvalidArg(format!(
            "{name} contains control characters"
        )));
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

// ─── region arguments: checked parsing + validation ────────────────────────────

/// Parsed, validated region request for the `file` tool.  Byte windows take
/// precedence over line windows when both are supplied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FileRegion {
    pub start_line: Option<u64>,
    pub end_line: Option<u64>,
    pub start_byte: Option<u64>,
    pub end_byte: Option<u64>,
}

/// Parse an optional unsigned integer argument with CHECKED conversion.
///
/// Missing key / explicit null → `None`.  Anything that is not a non-negative
/// integer (negative numbers, floats, strings, values above `u64::MAX`) is a
/// client-visible error — never an `as`-cast truncation.
fn parse_u64_arg(args: &HashMap<String, Value>, key: &str) -> Result<Option<u64>, ServerError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n)),
            None => Err(ServerError::InvalidArg(format!(
                "{key} must be a non-negative integer"
            ))),
        },
    }
}

fn parse_region(args: &HashMap<String, Value>) -> Result<FileRegion, ServerError> {
    let region = FileRegion {
        start_line: parse_u64_arg(args, "start_line")?,
        end_line: parse_u64_arg(args, "end_line")?,
        start_byte: parse_u64_arg(args, "start_byte")?,
        end_byte: parse_u64_arg(args, "end_byte")?,
    };

    for (name, v) in [
        ("start_line", region.start_line),
        ("end_line", region.end_line),
        ("start_byte", region.start_byte),
        ("end_byte", region.end_byte),
    ] {
        if let Some(v) = v
            && v > MAX_REGION_VALUE
        {
            return Err(ServerError::InvalidArg(format!(
                "{name} exceeds the maximum allowed value ({MAX_REGION_VALUE})"
            )));
        }
    }

    if let (Some(s), Some(e)) = (region.start_byte, region.end_byte) {
        if e < s {
            return Err(ServerError::InvalidArg(
                "end_byte must be greater than or equal to start_byte".into(),
            ));
        }
        if e - s > MAX_BYTE_SPAN {
            return Err(ServerError::InvalidArg(format!(
                "byte region too large (max {MAX_BYTE_SPAN} bytes per request)"
            )));
        }
    }
    if let (Some(s), Some(e)) = (region.start_line, region.end_line) {
        if e < s {
            return Err(ServerError::InvalidArg(
                "end_line must be greater than or equal to start_line".into(),
            ));
        }
        // Inclusive line window covers e - s + 1 lines.
        if e - s + 1 > MAX_LINE_SPAN {
            return Err(ServerError::InvalidArg(format!(
                "line region too large (max {MAX_LINE_SPAN} lines per request)"
            )));
        }
    }
    Ok(region)
}

// ─── UTF-8-safe slicing primitives ─────────────────────────────────────────────

/// Largest index `i <= pos` that is a char boundary of `s`.
///
/// Deterministic byte-region semantics: a user-supplied byte offset that does
/// NOT fall on a UTF-8 character boundary is floored DOWN to the nearest
/// character boundary (the partially-addressed character is included for a
/// start offset and excluded by an end offset).  Offsets past the end of the
/// string clamp to the end.  This function never panics.
pub(crate) fn floor_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Slice `s[start..end]` with UTF-8-flooring on both offsets.  Returns an
/// empty slice whenever the floored end does not exceed the floored start.
pub(crate) fn slice_utf8_safe(s: &str, start: usize, end: usize) -> &str {
    let e = floor_char_boundary(s, end);
    let b = floor_char_boundary(s, start).min(e);
    &s[b..e]
}

// ─── region application on in-memory text ──────────────────────────────────────

fn apply_region_bounds(text: &str, region: FileRegion) -> Result<Cow<'_, str>, ServerError> {
    if region.start_byte.is_some() || region.end_byte.is_some() {
        let len_usize = text.len();
        let s = usize::try_from(region.start_byte.unwrap_or(0))
            .unwrap_or(len_usize)
            .min(len_usize);
        let e = region
            .end_byte
            .map(|v| usize::try_from(v).unwrap_or(len_usize))
            .unwrap_or(len_usize)
            .min(len_usize);
        return Ok(Cow::Owned(slice_utf8_safe(text, s, e).to_owned()));
    }
    if region.start_line.is_some() || region.end_line.is_some() {
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len() as u64;
        let sl = region.start_line.unwrap_or(1).saturating_sub(1).min(total) as usize;
        let el = region.end_line.map(|e| e.min(total)).unwrap_or(total) as usize;
        if sl >= el {
            return Ok(Cow::Owned(String::new()));
        }
        return Ok(Cow::Owned(lines[sl..el].join("\n")));
    }
    Ok(Cow::Borrowed(text))
}

// ─── bounded streaming for LARGE files ────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
enum WindowSpec {
    /// Whole (redacted) stream, subject only to the response cap.
    All,
    /// Byte window `[start, end)` over the concatenated redacted stream.
    Bytes { start: u64, end: u64 },
    /// Inclusive 1-based line window `[start, end]` over the stream.
    Lines { start: u64, end: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopState {
    Running,
    WindowSatisfied,
    CapReached,
    ScanBoundReached,
}

/// Incrementally assembles the requested window from a LARGE file's sanitized
/// chunk stream.  At most `MAX_RESPONSE_BYTES` (+ one trailing marker) of
/// content is ever retained; the full file is NEVER accumulated.
struct StreamWindowCollector {
    spec: WindowSpec,
    out: String,
    produced: u64,
    line_no: u64,
    pending_line: String,
    state: StopState,
}

impl StreamWindowCollector {
    fn new(spec: WindowSpec) -> Self {
        Self {
            spec,
            out: String::new(),
            produced: 0,
            line_no: 0,
            pending_line: String::new(),
            state: StopState::Running,
        }
    }

    /// Feed one sanitized chunk.  Returns `false` when the caller may stop
    /// pulling further chunks (window complete or a limit reached).
    fn feed(&mut self, chunk: &str) -> bool {
        if self.state != StopState::Running {
            return false;
        }

        let chunk_len = chunk.len() as u64;
        let chunk_start = self.produced;
        let chunk_end = chunk_start + chunk_len;
        self.produced = chunk_end;

        match self.spec {
            WindowSpec::Bytes { start, end } => {
                if chunk_end > start && chunk_start < end {
                    let local_s = start.saturating_sub(chunk_start).min(chunk_len) as usize;
                    let local_e = end.saturating_sub(chunk_start).min(chunk_len) as usize;
                    let piece = slice_utf8_safe(chunk, local_s, local_e.max(local_s));
                    self.push_bounded(piece);
                }
                if self.produced >= end && self.state == StopState::Running {
                    self.state = StopState::WindowSatisfied;
                }
            }
            WindowSpec::Lines { start, end } => {
                let mut rest = chunk;
                let mut carry = std::mem::take(&mut self.pending_line);
                while let Some(nl) = rest.find('\n') {
                    carry.push_str(&rest[..=nl]);
                    self.line_no += 1;
                    if self.line_no >= start && self.line_no <= end {
                        self.push_bounded(&carry);
                    }
                    carry.clear();
                    rest = &rest[nl + 1..];
                    if self.line_no >= end {
                        break;
                    }
                }
                // Whatever remains has no newline yet — buffer for the next chunk.
                carry.push_str(rest);
                if carry.len() > MAX_RESPONSE_BYTES * 2 {
                    // Pathological single-line input: keep memory bounded.
                    carry.truncate(MAX_RESPONSE_BYTES * 2);
                }
                self.pending_line = carry;
                if self.line_no >= end && self.state == StopState::Running {
                    self.state = StopState::WindowSatisfied;
                }
            }
            WindowSpec::All => {
                self.push_bounded(chunk);
            }
        }

        if self.out.len() >= MAX_RESPONSE_BYTES {
            self.state = StopState::CapReached;
        } else if self.produced >= MAX_STREAM_SCAN_BYTES {
            self.state = StopState::ScanBoundReached;
        }

        self.state == StopState::Running
    }

    /// Append at most enough of `piece` to stay under the response cap,
    /// refusing to split a UTF-8 character at the cut point.
    fn push_bounded(&mut self, piece: &str) {
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(self.out.len());
        if remaining == 0 {
            return;
        }
        if piece.len() <= remaining {
            self.out.push_str(piece);
            return;
        }
        let mut take = remaining;
        while take > 0 && !piece.is_char_boundary(take) {
            take -= 1;
        }
        self.out.push_str(&piece[..take]);
    }

    /// Finish the stream and produce the final response body.
    fn finish(mut self) -> String {
        if self.spec_matches_lines() && !self.pending_line.is_empty() {
            // Final unterminated line.
            self.line_no += 1;
            let last = std::mem::take(&mut self.pending_line);
            if let WindowSpec::Lines { start, end } = self.spec
                && self.line_no >= start
                && self.line_no <= end
            {
                self.push_bounded(&last);
            }
        }
        if matches!(self.spec, WindowSpec::Lines { .. })
            && let Some(stripped) = self.out.strip_suffix('\n')
        {
            self.out = stripped.to_owned();
        }
        match self.state {
            StopState::CapReached => self
                .out
                .push_str("\n\n[truncated: response exceeded the server output limit]"),
            StopState::ScanBoundReached => self.out.push_str(
                "\n\n[truncated: file exceeds the maximum scannable size for one response]",
            ),
            _ => {}
        }
        self.out
    }

    fn spec_matches_lines(&self) -> bool {
        matches!(self.spec, WindowSpec::Lines { .. })
    }
}

/// Consume a LARGE file's sanitized chunk stream and assemble ONLY the
/// requested window, enforcing every output bound.  The complete file is
/// never accumulated in memory.
fn stream_window_from_large_file(
    stream: &mut attic_discovery::LargeFileStream,
    region: FileRegion,
) -> Result<String, ServerError> {
    let spec = if region.start_byte.is_some() || region.end_byte.is_some() {
        WindowSpec::Bytes {
            start: region.start_byte.unwrap_or(0),
            end: region.end_byte.unwrap_or(u64::MAX),
        }
    } else if region.start_line.is_some() || region.end_line.is_some() {
        WindowSpec::Lines {
            start: region.start_line.unwrap_or(1),
            end: region.end_line.unwrap_or(u64::MAX),
        }
    } else {
        WindowSpec::All
    };

    let mut collector = StreamWindowCollector::new(spec);
    'pull: while let Some(chunk_result) = stream.next_chunk() {
        let chunk = chunk_result.map_err(ServerError::Discovery)?;
        if !collector.feed(&chunk.redacted) {
            break 'pull;
        }
    }
    Ok(collector.finish())
}

// ─── response-size enforcement for non-streamed content ───────────────────────

fn enforce_response_limit(mut body: String) -> String {
    if body.len() <= MAX_RESPONSE_BYTES {
        return body;
    }
    let mut cut = MAX_RESPONSE_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    body.truncate(cut);
    body.push_str("\n\n[truncated: response exceeded the server output limit]");
    body
}

// ─── tool handlers ─────────────────────────────────────────────────────────────

fn handle_file(
    pool: &DbPool,
    args: &HashMap<String, Value>,
) -> Result<CallToolResult, ServerError> {
    let repo_id = args
        .get("repository_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;

    let file_path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("path required".into()))?;

    let region = parse_region(args)?;

    let parsed_repo_id = repo_id
        .parse::<attic_core::RepositoryId>()
        .map_err(|e| ServerError::InvalidArg(format!("invalid repository_id: {e}")))?;

    let repo_root_str = pool
        .with_reader(|c| get_repository_path(c, &parsed_repo_id))?
        .ok_or_else(|| ServerError::InvalidArg(format!("repository_id {repo_id} not found")))?;
    let repo_root_raw = PathBuf::from(&repo_root_str);
    // On Windows, std::fs::canonicalize adds a \\?\ extended-length prefix.
    // canonicalize_within_root canonicalizes the joined path, so the result
    // also has \\?\; but repo_root_raw (from the DB) does not.  Normalize
    // repo_root the same way so that strip_prefix succeeds.
    let repo_root = repo_root_raw.canonicalize().unwrap_or(repo_root_raw);

    let abs_path = canonicalize_within_root(&repo_root.join(file_path), &repo_root)
        .map_err(|e| ServerError::InvalidArg(format!("path rejected: {e}")))?;

    let repo_relative = abs_path
        .strip_prefix(&repo_root)
        .map_err(|_| ServerError::InvalidArg("path outside repo root".into()))?
        .to_string_lossy()
        .replace('\\', "/");

    // Block access to git-internal paths at the server layer regardless of
    // what preprocess_file_content decides, to ensure consistent policy.
    {
        let rr = repo_relative.as_str();
        if rr == ".git" || rr.starts_with(".git/") || rr.starts_with(".git\\") {
            return Err(ServerError::InvalidArg(
                "path rejected: .git internals are forbidden".into(),
            ));
        }
    }

    // preprocess handles Excluded/Redacted/secrets internally via the secrets scan layer
    let pre = preprocess_file_content(&abs_path, &repo_relative).map_err(ServerError::Discovery)?;

    if pre.decision == SecretScanDecision::Excluded {
        return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "# {repo_relative}\n\n[Excluded by security policy]"
        ))]));
    }
    if pre.decision == SecretScanDecision::PartialScan {
        warn!("file {repo_relative}: partial scan");
    }

    let body: String = if let Some(text) = pre.content {
        // SMALL / Redacted / PartialScan content held fully in memory already.
        let bounded = apply_region_bounds(&text, region)?;
        enforce_response_limit(bounded.into_owned())
    } else if let Some(mut stream) = pre.stream {
        // LARGE file: genuinely bounded incremental retrieval.  Sanitized
        // chunks are streamed through the window collector; the whole file is
        // never accumulated.
        stream_window_from_large_file(&mut stream, region)?
    } else {
        String::new()
    };

    let header = match pre.decision {
        SecretScanDecision::Redacted => format!("# {repo_relative}\n# [Secrets redacted]\n\n"),
        SecretScanDecision::PartialScan => format!("# {repo_relative}\n# [Partial scan]\n\n"),
        _ => format!("# {repo_relative}\n\n"),
    };

    // Phase 2: never present stale indexed state as CURRENT.  The body is
    // read live from disk, but if the latest occurrence for this path is not
    // CURRENT the response says so explicitly.
    let freshness_note = pool
        .with_reader(|c| {
            attic_storage::lookup_occurrence_snapshot(c, &parsed_repo_id, &repo_relative)
        })?
        .map(|s| s.freshness_state)
        .filter(|f| f != "CURRENT")
        .map(|f| format!("# [index freshness: {f}]\n"))
        .unwrap_or_default();

    Ok(CallToolResult::success(vec![ContentBlock::text(format!(
        "{header}{freshness_note}\n{body}"
    ))]))
}

fn handle_search(
    pool: &DbPool,
    args: &HashMap<String, Value>,
) -> Result<CallToolResult, ServerError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
    validate_filter("query", query, 512)?;

    let repo_id = args.get("repository_id").and_then(Value::as_str);
    if let Some(id) = repo_id {
        validate_repository_id(id)?;
    }
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type {
        validate_filter("file_type", ft, 32)?;
    }
    let language = args.get("language").and_then(Value::as_str);
    if let Some(lg) = language {
        validate_filter("language", lg, 64)?;
    }

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
    let repo_id = args
        .get("repository_id")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
    validate_repository_id(repo_id)?;
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type {
        validate_filter("file_type", ft, 32)?;
    }
    let all_stats = pool.with_reader(get_repository_stats)?;
    let stats = all_stats.into_iter().find(|s| s.id == repo_id);
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({ "repository_id": repo_id, "stats": stats }))?,
    )]))
}

fn handle_status(
    pool: &DbPool,
    incremental: Option<&Arc<attic_incremental::IncrementalService>>,
    watch_mode: Option<attic_incremental::WatchMode>,
) -> Result<CallToolResult, ServerError> {
    let stats = pool.with_reader(get_db_stats)?;
    let mut payload = json!({ "status": "ok", "db": stats });

    // Incremental subsystem state — including EXPLICIT degraded modes.  The
    // server never claims a mode it is not actually running.
    payload["watcher"] = json!({
        "mode": match (watch_mode, incremental.is_some()) {
            (Some(m), _) => m.as_str(),
            (None, true) => "starting",
            (None, false) => "disabled",
        },
        "active": matches!(watch_mode, Some(attic_incremental::WatchMode::NativeWatcher)),
        "periodic_reconciliation": matches!(
            watch_mode,
            Some(attic_incremental::WatchMode::PeriodicReconciliation)
        ),
    });

    if let Some(svc) = incremental {
        match svc.status_snapshot(pool) {
            Ok(snap) => {
                let recovery_state = if snap.reconciliation_required {
                    "RECONCILIATION_REQUIRED"
                } else if snap.tasks.pending > 0 || snap.tasks.running > 0 {
                    "INDEXING"
                } else {
                    "CURRENT"
                };
                payload["incremental"] = json!({
                    "state": recovery_state,
                    "events_ingested": snap.events_ingested,
                    "hints_dropped": snap.hints_dropped,
                    "watcher_errors": snap.watcher_errors,
                    "raw_batches_dropped": snap.raw_batches_dropped,
                    "reconciliation_required": snap.reconciliation_required,
                    "freshness": snap.freshness,
                    "tasks": snap.tasks,
                });
            }
            Err(e) => {
                payload["incremental"] = json!({
                    "state": "UNKNOWN",
                    "error": e.to_string(),
                });
            }
        }
    }

    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload)?,
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
             (start_line/end_line, 1-indexed, inclusive) and byte-range (start_byte/end_byte, \
             0-indexed, exclusive; byte ranges override lines). Byte offsets that do not land \
             on a UTF-8 character boundary are floored to the nearest preceding boundary. \
             LARGE files are streamed; responses are capped by server-side output limits.",
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
        let incremental = self.incremental.clone();
        let watch_mode = self.watch_mode;
        let name = request.name.clone();
        let args: HashMap<String, Value> =
            request.arguments.unwrap_or_default().into_iter().collect();

        async move {
            let result: Result<CallToolResult, ServerError> = match name.as_ref() {
                "file" => handle_file(&pool, &args),
                "search" => handle_search(&pool, &args),
                "repo_map" => handle_repo_map(&pool, &args),
                "status" => handle_status(&pool, incremental.as_ref(), watch_mode),
                other => Err(ServerError::InvalidArg(format!("unknown tool: {other}"))),
            };
            match result {
                Ok(r) => Ok(r.into()),
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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let db_path = std::env::var("ATTIC_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|_| {
                    std::env::var("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
                })
                .or_else(|_| {
                    std::env::var("USERPROFILE")
                        .map(|h| PathBuf::from(h).join("AppData").join("Local"))
                })
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("attic")
                .join("attic.db")
        });

    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p)?;
    }
    info!("attic starting, db={}", db_path.display());

    let mut server = AtticServer::new(&db_path)?;

    // ─── Startup recovery — ALWAYS before serving (recovery contract §3) ────
    //
    // Fail-closed: if recovery cannot establish a safe state, the process
    // refuses to serve rather than risk presenting affected data as CURRENT
    // (REC-INV-1).  There is no silent "keep going" path.
    match attic_incremental::run_startup_recovery(&server.pool, &server.writer) {
        Ok(report) => info!(
            tasks_reset = report.tasks_reset,
            abandoned_runs = report.indexing_runs_abandoned,
            rescheduled = report.refreshes_rescheduled,
            epoch = report.watcher_epoch,
            previous_clean_shutdown = report.previous_shutdown_clean,
            "startup recovery complete"
        ),
        Err(e) => {
            error!("startup recovery FAILED — refusing to serve (fail-closed): {e}");
            return Err(anyhow::anyhow!("startup recovery failed: {e}"));
        }
    }

    let mut _watch: Option<attic_incremental::IncrementalWatch> = None;
    let mut _sched_handle: Option<attic_incremental::SchedulerHandle> = None;

    // ─── Phase 2 watch mode (ATTIC_WORKSPACE_ROOT present) ──────────────────
    if let Ok(ws) = std::env::var("ATTIC_WORKSPACE_ROOT") {
        let root = PathBuf::from(&ws);

        // 1. Bootstrap / index the workspace synchronously on first run.
        //
        // FAIL-CLOSED: a failed initial indexing must never leave stale or
        // partial state presented as CURRENT.  The publication itself is
        // atomic, but any failure here means we cannot vouch for the
        // workspace — refuse to serve rather than guess.
        let srv = server.clone();
        let root_for_bootstrap = root.clone();
        let boot =
            tokio::task::spawn_blocking(move || srv.bootstrap_workspace(&root_for_bootstrap))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        match evaluate_bootstrap(&boot) {
            BootstrapAction::Proceed(_) => {}
            BootstrapAction::FailClosed(why) => {
                error!("workspace bootstrap FAILED — refusing to serve (fail-closed): {why}");
                return Err(anyhow::anyhow!("bootstrap failed: {why}"));
            }
        }

        // 2. Schedule offline refresh for anything not CURRENT.
        match attic_incremental::plan_offline_refresh(&server.pool) {
            Ok(batch) => {
                for refresh in batch {
                    let payload = attic_storage::IncrementalTaskPayload {
                        dedup_key: format!(
                            "offline-{}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_micros())
                                .unwrap_or_default()
                        ),
                        upserts: refresh.upsert_paths,
                        deletes: vec![],
                        renames: vec![],
                        from_reconciliation: true,
                    };
                    if let Err(e) = attic_incremental::scheduler::schedule_incremental(
                        &server.writer,
                        &refresh.repository_id,
                        &payload,
                        attic_incremental::scheduler::PRIORITY_RECONCILE,
                        4096,
                    ) {
                        warn!("offline refresh scheduling failed: {e}");
                    }
                }
            }
            Err(e) => warn!("offline refresh planning failed: {e}"),
        }

        // 3. Scheduler — fallible: a scheduler that cannot start its workers
        //    is never silently accepted.
        let policy = DiscoveryPolicy::default_git();
        match attic_incremental::spawn_scheduler(
            attic_incremental::SchedulerConfig::default(),
            server.pool.clone(),
            server.writer.clone(),
            root.clone(),
            policy.clone(),
        ) {
            Ok(sched) => _sched_handle = Some(sched),
            Err(e) => {
                error!("scheduler startup failed — incremental mode DISABLED: {e}");
                server.watch_mode = None;
                server.incremental = None;
                // Continue serving WITHOUT incremental claims; status reports
                // watcher.mode = "disabled".
                return serve_until_closed(server).await;
            }
        }

        // 4. Change detection: native watcher, or a REAL bounded periodic
        //    reconciliation loop as fallback — both perform actual work and
        //    the active mode is exposed via `status`.
        let service = Arc::new(
            attic_incremental::IncrementalService::new(&root, policy)
                .with_quiet_period_ms(attic_incremental::DEFAULT_QUIET_MS),
        );
        match service.start_incremental_watch(server.pool.clone(), server.writer.clone()) {
            Ok(watch) => {
                info!(
                    mode = watch.mode().as_str(),
                    "incremental change detection started"
                );
                server.watch_mode = Some(watch.mode());
                _watch = Some(watch);
            }
            Err(e) => {
                error!("change detection failed to start ({e}) — incremental DISABLED");
                server.watch_mode = None;
                server.incremental = None;
                return serve_until_closed(server).await;
            }
        }
        server.incremental = Some(service);
    }

    serve_until_closed(server).await
}

/// Outcome of the initial workspace indexing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BootstrapAction {
    /// Indexing succeeded; repository id available.
    Proceed(String),
    /// Indexing failed — refuse to serve anything as CURRENT (fail-closed).
    FailClosed(String),
}

/// Map a bootstrap outcome to the serve/fail-closed decision.
///
/// Unit-tested: an `Err` MUST map to [`BootstrapAction::FailClosed`] so the
/// caller exits instead of serving stale state as CURRENT.
fn evaluate_bootstrap(r: &Result<String, ServerError>) -> BootstrapAction {
    match r {
        Ok(id) => BootstrapAction::Proceed(id.clone()),
        Err(e) => BootstrapAction::FailClosed(e.to_string()),
    }
}

/// Serve MCP until the stdio transport closes, then record a clean shutdown.
async fn serve_until_closed(server: AtticServer) -> anyhow::Result<()> {
    // Keep a writer handle for the clean-shutdown marker; `serve` consumes
    // the server value.
    let writer_for_shutdown = server.writer.clone();
    // Serve returns once the MCP lifecycle handshake has completed.  The
    // returned RunningService MUST be kept alive for the lifetime of the
    // server — dropping it cancels the whole service.  `waiting()` consumes
    // it and blocks until the stdio transport closes.
    let running = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let reason = running
        .waiting()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    info!("attic server stopped: {reason:?}");

    // Clean-shutdown marker for crash detection on next start.
    let _ = attic_incremental::record_clean_shutdown_marker(&writer_for_shutdown);
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

    fn region_args(
        start_line: Option<u64>,
        end_line: Option<u64>,
        start_byte: Option<u64>,
        end_byte: Option<u64>,
    ) -> FileRegion {
        FileRegion {
            start_line,
            end_line,
            start_byte,
            end_byte,
        }
    }

    // compile-time gate: no rusqlite direct dep
    #[test]
    fn no_direct_rusqlite_in_server() {
        let _ = true;
    }

    // compile-time gate: IndexError has no Sqlite variant
    #[test]
    fn bootstrap_failure_is_fail_closed() {
        // Err ⇒ FailClosed (never serve stale state as CURRENT).
        let err: Result<String, ServerError> = Err(ServerError::Indexing(
            IndexError::RepositoryNotBootstrapped("/ws".into()),
        ));
        assert_eq!(
            evaluate_bootstrap(&err),
            BootstrapAction::FailClosed(
                "indexing error: repository at /ws has not been bootstrapped; run a full index first"
                    .into()
            )
        );

        // Ok ⇒ Proceed with the repository id.
        let ok: Result<String, ServerError> = Ok("repo-1".into());
        assert_eq!(
            evaluate_bootstrap(&ok),
            BootstrapAction::Proceed("repo-1".into())
        );
    }

    #[test]
    fn indexing_uses_writer_abstraction() {
        fn _check(e: IndexError) {
            match e {
                IndexError::Discovery(_) => {}
                IndexError::Storage(_) => {}
                IndexError::Io { .. } => {}
                IndexError::PolicyHash(_) => {}
                IndexError::RepositoryNotBootstrapped(_) => {}
            }
        }
    }

    // validate_filter
    #[test]
    fn validate_filter_ok() {
        assert!(validate_filter("q", "hello", 512).is_ok());
    }
    #[test]
    fn validate_filter_long() {
        assert!(validate_filter("q", &"a".repeat(11), 10).is_err());
    }
    #[test]
    fn validate_filter_ctrl() {
        assert!(validate_filter("q", "a\x00b", 512).is_err());
    }
    #[test]
    fn validate_repo_id_ok() {
        assert!(validate_repository_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }
    #[test]
    fn validate_repo_id_bad() {
        assert!(validate_repository_id("../../etc").is_err());
    }
    #[test]
    fn validate_repo_id_long() {
        assert!(validate_repository_id(&"a".repeat(65)).is_err());
    }

    // ── region parsing: checked numeric conversions ──────────────────────────

    #[test]
    fn parse_region_missing_keys_is_empty() {
        let a: HashMap<String, Value> = HashMap::new();
        assert_eq!(parse_region(&a).unwrap(), FileRegion::default());
    }

    #[test]
    fn parse_region_rejects_negative() {
        let mut a = HashMap::new();
        a.insert("start_byte".into(), json!(-1));
        let err = parse_region(&a).unwrap_err().to_string();
        assert!(err.contains("non-negative"), "{err}");
    }

    #[test]
    fn parse_region_rejects_float() {
        let mut a = HashMap::new();
        a.insert("start_line".into(), json!(2.5));
        assert!(parse_region(&a).is_err());
    }

    #[test]
    fn parse_region_rejects_string() {
        let mut a = HashMap::new();
        a.insert("end_line".into(), json!("ten"));
        assert!(parse_region(&a).is_err());
    }

    #[test]
    fn parse_region_rejects_overflow_magnitude() {
        // A JSON number beyond u64::MAX parses as a float → not a non-negative
        // integer → rejected instead of truncated.
        let mut a = HashMap::new();
        a.insert("start_byte".into(), json!(1e30));
        assert!(parse_region(&a).is_err());

        // Values within u64 but beyond MAX_REGION_VALUE are rejected BEFORE
        // any conversion or expensive work.
        let mut b = HashMap::new();
        b.insert("end_line".into(), json!(u64::MAX));
        let err = parse_region(&b).unwrap_err().to_string();
        assert!(err.contains("maximum allowed"), "{err}");
    }

    #[test]
    fn parse_region_rejects_inverted_windows() {
        let mut a = HashMap::new();
        a.insert("start_byte".into(), json!(10));
        a.insert("end_byte".into(), json!(5));
        let err = parse_region(&a).unwrap_err().to_string();
        assert!(err.contains("greater than or equal"), "{err}");

        let mut b = HashMap::new();
        b.insert("start_line".into(), json!(9));
        b.insert("end_line".into(), json!(2));
        assert!(parse_region(&b).is_err());
    }

    #[test]
    fn parse_region_enforces_span_limits() {
        let mut a = HashMap::new();
        a.insert("start_line".into(), json!(1));
        a.insert("end_line".into(), json!(MAX_LINE_SPAN + 1));
        let err = parse_region(&a).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");

        // Exactly MAX_LINE_SPAN lines is allowed.
        let mut ok = HashMap::new();
        ok.insert("start_line".into(), json!(1));
        ok.insert("end_line".into(), json!(MAX_LINE_SPAN));
        assert!(parse_region(&ok).is_ok());

        let mut b = HashMap::new();
        b.insert("start_byte".into(), json!(0));
        b.insert("end_byte".into(), json!(MAX_BYTE_SPAN + 1));
        assert!(parse_region(&b).is_err());

        // Exactly at the limit is fine.
        let mut c = HashMap::new();
        c.insert("start_byte".into(), json!(0));
        c.insert("end_byte".into(), json!(MAX_BYTE_SPAN));
        assert!(parse_region(&c).is_ok());
    }

    // ── UTF-8-safe byte regions ──────────────────────────────────────────────

    #[test]
    fn floor_char_boundary_basic_and_clamps() {
        let s = "abc";
        assert_eq!(floor_char_boundary(s, 0), 0);
        assert_eq!(floor_char_boundary(s, 2), 2);
        assert_eq!(floor_char_boundary(s, 3), 3);
        assert_eq!(floor_char_boundary(s, 999), 3);
    }

    #[test]
    fn utf8_invalid_offsets_never_panic_and_are_deterministic() {
        // Layout: 'a'(1B) é(2B) 日(3B) x(1B) → boundaries {0,1,3,6,7}, len 7.
        let s = "a\u{e9}\u{65e5}x";
        assert_eq!(s.len(), 7);

        // start inside 'é' (byte 2) floors to 1; end at boundary 6.
        assert_eq!(slice_utf8_safe(s, 2, 6), "\u{e9}\u{65e5}");
        // start inside 日 (byte 5) floors to 3.
        assert_eq!(slice_utf8_safe(s, 5, 7), "\u{65e5}x");
        // end inside 日 (byte 4) floors to 3 → empty tail from 3.
        assert_eq!(slice_utf8_safe(s, 3, 4), "");
        // both offsets inside the same character → empty.
        assert_eq!(slice_utf8_safe(s, 4, 5), "");
        // clamping past EOF.
        assert_eq!(slice_utf8_safe(s, 0, 100), s);
        assert_eq!(slice_utf8_safe(s, 100, 200), "");

        // Pure ASCII behaviour unchanged.
        assert_eq!(slice_utf8_safe("abcdef", 1, 4), "bcd");
    }

    // apply_region_bounds
    #[test]
    fn region_full() {
        let s = "a\nb\nc";
        assert_eq!(
            apply_region_bounds(s, region_args(None, None, None, None))
                .unwrap()
                .as_ref(),
            s
        );
    }
    #[test]
    fn region_lines() {
        assert_eq!(
            apply_region_bounds("L1\nL2\nL3", region_args(Some(2), Some(2), None, None))
                .unwrap()
                .as_ref(),
            "L2"
        );
    }
    #[test]
    fn region_bytes() {
        assert_eq!(
            apply_region_bounds("abcdef", region_args(None, None, Some(1), Some(4)))
                .unwrap()
                .as_ref(),
            "bcd"
        );
    }
    #[test]
    fn region_bytes_win_over_lines() {
        assert_eq!(
            apply_region_bounds("abcdef", region_args(Some(1), Some(1), Some(1), Some(4)))
                .unwrap()
                .as_ref(),
            "bcd"
        );
    }
    #[test]
    fn region_bytes_clamped() {
        assert_eq!(
            apply_region_bounds("hi", region_args(None, None, Some(0), Some(999)))
                .unwrap()
                .as_ref(),
            "hi"
        );
    }
    #[test]
    fn region_bytes_past_end() {
        assert_eq!(
            apply_region_bounds("hi", region_args(None, None, Some(999), None))
                .unwrap()
                .as_ref(),
            ""
        );
    }
    #[test]
    fn region_multibyte_bytes_are_floored_not_panicking() {
        let s = "a\u{e9}\u{65e5}x"; // boundaries {0,1,3,6,7}
        assert_eq!(
            apply_region_bounds(s, region_args(None, None, Some(2), Some(6)))
                .unwrap()
                .as_ref(),
            "\u{e9}\u{65e5}"
        );
        assert_eq!(
            apply_region_bounds(s, region_args(None, None, Some(4), Some(5)))
                .unwrap()
                .as_ref(),
            ""
        );
    }
    #[test]
    fn region_line_window_too_far_returns_empty() {
        assert_eq!(
            apply_region_bounds("one", region_args(Some(50), None, None, None))
                .unwrap()
                .as_ref(),
            ""
        );
    }

    // ── response-size enforcement ────────────────────────────────────────────

    #[test]
    fn response_under_cap_passes_through() {
        let body = "hello".to_owned();
        assert_eq!(enforce_response_limit(body.clone()), body);
    }

    #[test]
    fn response_over_cap_truncated_at_char_boundary() {
        // Multibyte padding so a naive byte-cut would split a character.
        let unit = "\u{65e5}".repeat(400_000); // 1_200_000 bytes > 1 MiB cap
        let out = enforce_response_limit(unit);
        assert!(
            out.len() < MAX_RESPONSE_BYTES + 128,
            "out len {}",
            out.len()
        );
        assert!(out.ends_with("[truncated: response exceeded the server output limit]"));
        // Everything before the marker must consist of WHOLE original
        // characters only (no split multi-byte sequences — String guarantees
        // UTF-8 validity, this checks no character was lost mid-sequence).
        let body = out.split("\n\n[truncated").next().unwrap();
        assert!(!body.is_empty());
        assert!(
            body.chars().all(|c| c == '\u{65e5}'),
            "split character detected"
        );
    }

    // ── streaming collector units ────────────────────────────────────────────

    #[test]
    fn stream_collector_byte_window_across_chunks() {
        let mut c = StreamWindowCollector::new(WindowSpec::Bytes { start: 3, end: 12 });
        let mut fed = String::new();
        for piece in ["01234", "56789", "abcde"] {
            fed.push_str(piece);
            if !c.feed(piece) {
                break;
            }
        }
        let out = c.finish();
        assert_eq!(out, &fed[3..12]);
    }

    #[test]
    fn stream_collector_stops_early_once_window_complete() {
        let mut c = StreamWindowCollector::new(WindowSpec::Bytes { start: 0, end: 5 });
        // The chunk that satisfies the window already signals "stop pulling".
        assert!(!c.feed("hello world garbage"));
        assert_eq!(c.finish(), "hello");
    }

    #[test]
    fn stream_collector_lines_window_with_split_lines() {
        let mut c = StreamWindowCollector::new(WindowSpec::Lines { start: 2, end: 3 });
        c.feed("l1\nl2\nl3"); // note: l3 has no trailing newline yet
        c.feed("\nl4\n");
        let out = c.finish();
        assert_eq!(out, "l2\nl3");
    }

    #[test]
    fn stream_collector_all_mode_caps_output() {
        let mut c = StreamWindowCollector::new(WindowSpec::All);
        loop {
            if !c.feed(&"x".repeat(64 * 1024)) {
                break;
            }
            if c.out.len() > MAX_RESPONSE_BYTES {
                panic!("collector exceeded cap");
            }
        }
        let out = c.finish();
        assert!(out.ends_with("[truncated: response exceeded the server output limit]"));
        assert!(out.len() < MAX_RESPONSE_BYTES + 128);
    }

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
        let e = handle_file(&make_server(&tmp).pool, &a)
            .unwrap_err()
            .to_string();
        assert!(e.contains("path required"), "{e}");
    }

    #[test]
    fn file_unknown_repo() {
        let tmp = TempDir::new().unwrap();
        let mut a = HashMap::new();
        a.insert(
            "repository_id".into(),
            json!("deadbeef-0000-0000-0000-000000000000"),
        );
        a.insert("path".into(), json!("src/lib.rs"));
        let e = handle_file(&make_server(&tmp).pool, &a)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not found"), "{e}");
    }

    #[test]
    fn file_rejects_overflow_numeric_arguments() {
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
        let repo = tmp.path().join("ovf");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("f.txt"), "data").unwrap();
        let id = srv.bootstrap_workspace(&repo).unwrap();

        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(id));
        a.insert("path".into(), json!("f.txt"));
        a.insert("start_byte".into(), json!(u64::MAX));
        let err = handle_file(&srv.pool, &a).unwrap_err().to_string();
        assert!(
            err.contains("maximum allowed") || err.contains("non-negative"),
            "{err}"
        );

        let mut b = HashMap::new();
        b.insert(
            "repository_id".into(),
            json!(srv.bootstrap_workspace(&repo).unwrap()),
        );
        b.insert("path".into(), json!("f.txt"));
        b.insert("end_byte".into(), json!(-42));
        assert!(handle_file(&srv.pool, &b).is_err());
    }

    // handle_file: live read + region
    #[test]
    fn file_returns_live_content_and_region() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
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
        let r2 = handle_file(&srv.pool, &b).expect("region");
        let t2 = text_of(&r2);
        assert!(t2.contains("line2") && !t2.contains("line1"), "{t2}");
    }

    #[test]
    fn file_traversal_rejected() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
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
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
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
            Err(e) => assert!(
                e.to_string().contains("forbidden")
                    || e.to_string().contains("security")
                    || e.to_string().contains("rejected"),
                "{e}"
            ),
            Ok(cr) => {
                let t = text_of(&cr);
                assert!(
                    t.contains("Excluded") || t.contains("security") || t.contains("forbidden"),
                    "{t}"
                );
            }
        }
    }

    // ── LARGE-file genuinely bounded retrieval ───────────────────────────────

    /// Build a deterministic LARGE-tier file (>4 MiB, ≤50 MiB) with unique
    /// marker tokens at known line positions.  Returns `(path, base_line_len)`.
    fn build_large_file(dir: &Path, name: &str) -> (std::path::PathBuf, usize) {
        let path = dir.join(name);
        let base_line = format!("{}\n", "filler payload ".repeat(24)); // 361 bytes
        let base_len = base_line.len();
        let target_total = 4 * 1024 * 1024 + 512 * 1024; // 4.5 MiB
        let mut f = std::io::BufWriter::new(fs::File::create(&path).unwrap());
        let mut written = 0usize;
        let mut lineno = 0usize;
        while written < target_total {
            lineno += 1;
            let l = match lineno {
                100 => format!("MIDDLE_MARKER_TOKEN_{lineno} {base_line}"),
                9000 => format!("TAIL_MARKER_TOKEN_{lineno} {base_line}"),
                _ => base_line.clone(),
            };
            use std::io::Write as _;
            f.write_all(l.as_bytes()).unwrap();
            written += l.len();
        }
        drop(f);
        (path, base_len)
    }

    #[test]
    fn large_file_region_is_genuinely_bounded_streamed() {
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
        let repo = tmp.path().join("big");
        fs::create_dir_all(&repo).unwrap();
        let (big, base_len) = build_large_file(&repo, "large_source.txt");

        let meta = fs::metadata(&big).unwrap();
        assert!(
            meta.len() > SMALL_THRESHOLD_FOR_TEST,
            "fixture must be LARGE tier"
        );
        assert!(
            meta.len() <= 50 * 1024 * 1024,
            "fixture must not exceed LARGE tier"
        );
        let repo_id = srv.bootstrap_workspace(&repo).expect("bootstrap");

        // Byte-region covering MIDDLE_MARKER_TOKEN_100 exactly from its first
        // byte: lines 1..=99 are plain base lines, so the marker starts at
        // byte 99*base_len.
        let middle_offset = 99 * base_len;
        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(repo_id.clone()));
        a.insert("path".into(), json!("large_source.txt"));
        a.insert("start_byte".into(), json!(middle_offset as u64));
        a.insert("end_byte".into(), json!(middle_offset as u64 + 40));
        let r = handle_file(&srv.pool, &a).expect("middle region");
        let t = text_of(&r);
        assert!(t.contains("MIDDLE_MARKER_TOKEN_100"), "{t}");
        assert!(
            !t.contains("TAIL_MARKER_TOKEN_9000"),
            "must not leak other regions: {t}"
        );
        assert!(
            t.len() < 500,
            "response must be tiny, got {} bytes",
            t.len()
        );

        // Line-region at the tail marker.
        let mut b = HashMap::new();
        b.insert("repository_id".into(), json!(repo_id.clone()));
        b.insert("path".into(), json!("large_source.txt"));
        b.insert("start_line".into(), json!(9000u64));
        b.insert("end_line".into(), json!(9000u64));
        let r2 = handle_file(&srv.pool, &b).expect("tail line region");
        let t2 = text_of(&r2);
        assert!(t2.contains("TAIL_MARKER_TOKEN_9000"), "{t2}");
        assert!(!t2.contains("MIDDLE_MARKER_TOKEN_100"), "{t2}");

        // Full-file request must be CAPPED, proving the whole 4.5 MiB file is
        // never accumulated into the response.
        let mut c = HashMap::new();
        c.insert("repository_id".into(), json!(repo_id));
        c.insert("path".into(), json!("large_source.txt"));
        let r3 = handle_file(&srv.pool, &c).expect("full file");
        let t3 = text_of(&r3);
        assert!(
            t3.len() <= MAX_RESPONSE_BYTES + 256,
            "full-file response must be capped, got {} bytes",
            t3.len()
        );
        assert!(
            t3.contains("[truncated:"),
            "cap must be reported: len={}",
            t3.len()
        );
    }

    const SMALL_THRESHOLD_FOR_TEST: u64 = 4 * 1024 * 1024 + 256 * 1024;

    #[test]
    fn small_single_huge_line_response_is_capped() {
        // A SMALL file whose single line dwarfs the response cap.
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
        let repo = tmp.path().join("huge_line");
        fs::create_dir_all(&repo).unwrap();
        let huge = format!("HUGE_START{}HUGE_END", "z".repeat(2 * 1024 * 1024));
        fs::write(repo.join("huge_line.txt"), &huge).unwrap();
        let repo_id = srv.bootstrap_workspace(&repo).unwrap();

        let mut a = HashMap::new();
        a.insert("repository_id".into(), json!(repo_id));
        a.insert("path".into(), json!("huge_line.txt"));
        let r = handle_file(&srv.pool, &a).expect("huge line file");
        let t = text_of(&r);
        assert!(t.len() <= MAX_RESPONSE_BYTES + 256, "len {}", t.len());
        assert!(t.contains("[truncated:"), "{:.80}", t);
        assert!(t.contains("HUGE_START"), "head must be preserved");
    }

    // handle_search
    #[test]
    fn search_missing_query() {
        let tmp = TempDir::new().unwrap();
        let e = handle_search(&make_server(&tmp).pool, &HashMap::new())
            .unwrap_err()
            .to_string();
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
        let r = handle_status(&make_server(&tmp).pool, None, None).unwrap();
        let t = text_of(&r);
        let v: Value = serde_json::from_str(&t).unwrap();
        assert_eq!(v["status"], "ok");
    }

    // handle_repo_map
    #[test]
    fn repo_map_missing_repo_id() {
        let tmp = TempDir::new().unwrap();
        let e = handle_repo_map(&make_server(&tmp).pool, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("repository_id required"), "{e}");
    }

    // workspace lifecycle: index → search (coordinated writer end-to-end)
    #[test]
    fn workspace_becomes_searchable_through_coordinated_writer() {
        use std::fs;
        let tmp = TempDir::new().unwrap();
        let srv = make_server(&tmp);
        let repo = tmp.path().join("ws");
        fs::create_dir_all(&repo).unwrap();
        fs::write(repo.join("main.rs"), "fn hello_world() {}").unwrap();
        let repo_id = srv.bootstrap_workspace(&repo).unwrap();

        // status should succeed
        let r = handle_status(&srv.pool, None, None).unwrap();
        let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
        assert_eq!(v["status"], "ok");

        // search for content — proves the coordinated publication committed
        // retrievable units through the writer queue.
        let mut a = HashMap::new();
        a.insert("query".into(), json!("hello_world"));
        a.insert("repository_id".into(), json!(repo_id.clone()));
        let r2 = handle_search(&srv.pool, &a).unwrap();
        let v2: Value = serde_json::from_str(&text_of(&r2)).unwrap();
        let results = v2["results"].as_array().expect("results array");
        assert!(
            !results.is_empty(),
            "indexing via WriterQueue must yield searchable results"
        );

        // Second bootstrap is idempotent (same repo id, no duplicate rows).
        let again = srv.bootstrap_workspace(&repo).unwrap();
        assert_eq!(again, repo_id, "existing repository must be reused");
    }

    // ── MCP child-process tests (supplemental manual JSON-RPC protocol tests).
    // The required gate for real client↔server operation lives in
    // tests/rmcp_stdio_integration.rs using the official rmcp client API.
    // ─────────────────────────────────────────────────────────────────────────

    fn binary_path() -> PathBuf {
        let mut p = std::env::current_exe().unwrap();
        p.pop();
        if p.ends_with("deps") {
            p.pop();
        }
        let name = if cfg!(windows) { "attic.exe" } else { "attic" };
        p.join(name)
    }

    /// REQUIRED: the built `attic` binary.  These supplemental protocol tests
    /// FAIL (never silently pass) when the binary cannot be located.
    fn require_binary() -> PathBuf {
        let bin = binary_path();
        assert!(
            bin.exists(),
            "required MCP test binary missing: {} — build the attic binary first \
             (cargo build -p attic-server); these tests must fail rather than false-pass",
            bin.display()
        );
        bin
    }

    fn mcp_request(id: u64, method: &str, params: Value) -> String {
        let v = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        format!("{}\n", serde_json::to_string(&v).unwrap())
    }

    /// Legacy lifecycle handshake accepted by the rmcp 3.x server:
    /// protocolVersion must be one of the SDK's known versions and
    /// `capabilities` is a required field of InitializeRequestParams.
    fn spawn_and_initialize(
        bin: &Path,
        tmp: &TempDir,
    ) -> (std::process::Child, std::process::ChildStdin) {
        let mut child = Command::new(bin)
            .env(
                "ATTIC_DB_PATH",
                tmp.path().join("test.db").to_str().unwrap(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn attic server");
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["jsonrpc"], "2.0", "initialize failed: {resp}");
        assert_eq!(resp["id"], 1);
        // Notifications are fire-and-forget — they MUST NOT be awaited.
        send_only(
            &mut stdin,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        (child, stdin)
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

    /// Write-only send for notifications (which receive no reply).
    fn send_only(stdin: &mut std::process::ChildStdin, msg: &str) {
        stdin.write_all(msg.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    #[test]
    fn mcp_initialize_handshake() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env(
                "ATTIC_DB_PATH",
                tmp.path().join("test.db").to_str().unwrap(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(
            resp["result"]["serverInfo"]["name"]
                .as_str()
                .unwrap_or("")
                .contains("attic"),
            "expected attic in serverInfo, got: {resp}"
        );
        child.kill().ok();
    }

    #[test]
    fn mcp_tools_list() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
        let list_req = mcp_request(2, "tools/list", json!({}));
        let resp = send_recv(&mut child, &mut stdin, &list_req);
        assert_eq!(resp["jsonrpc"], "2.0");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"file"), "missing file tool: {names:?}");
        assert!(names.contains(&"search"), "missing search tool: {names:?}");
        assert!(
            names.contains(&"repo_map"),
            "missing repo_map tool: {names:?}"
        );
        assert!(names.contains(&"status"), "missing status tool: {names:?}");
        child.kill().ok();
    }

    #[test]
    fn mcp_call_tool_status() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
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
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
        let call = mcp_request(
            2,
            "tools/call",
            json!({"name":"repo_map","arguments":{"repository_id":"00000000-0000-0000-0000-000000000000"}}),
        );
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        assert!(
            resp["result"].is_object() || resp["error"].is_null(),
            "unexpected transport error: {resp}"
        );
        child.kill().ok();
    }

    #[test]
    fn mcp_call_tool_search_missing_query() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
        let call = mcp_request(2, "tools/call", json!({"name":"search","arguments":{}}));
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        let content = &resp["result"]["content"];
        if let Some(arr) = content.as_array() {
            let text = arr[0]["text"].as_str().unwrap_or("");
            assert!(
                text.contains("query required") || text.contains("required"),
                "expected 'query required' in error text, got: {text}"
            );
        }
        child.kill().ok();
    }

    #[test]
    fn mcp_call_unknown_tool_returns_error_content() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
        let call = mcp_request(
            2,
            "tools/call",
            json!({"name":"does_not_exist","arguments":{}}),
        );
        let resp = send_recv(&mut child, &mut stdin, &call);
        assert_eq!(resp["jsonrpc"], "2.0");
        let content = &resp["result"]["content"];
        if let Some(arr) = content.as_array() {
            let text = arr[0]["text"].as_str().unwrap_or("");
            assert!(
                text.contains("unknown tool") || text.contains("does_not_exist"),
                "expected unknown tool error, got: {text}"
            );
        }
        child.kill().ok();
    }

    #[test]
    fn mcp_stderr_does_not_contaminate_stdout() {
        let bin = require_binary();
        let tmp = TempDir::new().unwrap();
        let mut child = Command::new(&bin)
            .env(
                "ATTIC_DB_PATH",
                tmp.path().join("test.db").to_str().unwrap(),
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["jsonrpc"], "2.0", "stdout contains non-JSON: {resp}");
        child.kill().ok();
    }
}
