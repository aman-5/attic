use rmcp::model::Tool;
use serde_json::{Value, json};

// ─── schema helper ─────────────────────────────────────────────────────────────

fn json_schema(v: Value) -> std::sync::Arc<serde_json::Map<String, Value>> {
    std::sync::Arc::new(v.as_object().cloned().unwrap_or_default())
}

// ─── build the tool list once ──────────────────────────────────────────────────

pub(crate) fn make_tools() -> Vec<Tool> {
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
            "Hybrid search across indexed repositories: full-text (FTS5 query syntax) fused \
             with semantic nearest-neighbour candidates via reciprocal-rank fusion. Degrades \
             to lexical-only while semantic embeddings are unavailable or disabled. Each \
             result has a source_type (knowledge/documentation/code/config/test); \
             scope=\"knowledge\" returns only project knowledge.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "query":         {"type":"string","description":"FTS5 query (max 512 chars)"},
                    "repository_id": {"type":"string","description":"Limit results to this repository UUID"},
                    "file_type":     {"type":"string","description":"Filter by file extension (max 32)"},
                    "language":      {"type":"string","description":"Filter by detected language (max 64)"},
                    "scope":         {"type":"string","enum":["all","knowledge"],"description":"\"knowledge\" = only project knowledge (central knowledge folder first, then repository knowledge/ folders). Default \"all\""}
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
        Tool::new(
            "logging",
            "Runtime control of the persistent file log (<home>/logs/attic.log.*): \
             action=on (level defaults to info), off, level (requires level), or status. \
             Takes effect immediately in the already-running server — never requires a restart. \
             stderr output is unaffected and always on.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "action": {"type":"string","enum":["on","off","level","status"],"description":"Enable, disable, change the level of, or query file logging"},
                    "level": {"type":"string","enum":["error","warn","info","debug","trace"],"description":"File log verbosity for action=on|level (default info)"}
                },
                "required": ["action"]
            })),
        ),
        Tool::new(
            "debug_drain_task",
            "Debug/admin: claim and execute exactly one pending incremental indexing task \
             synchronously, bypassing the background scheduler's poll interval. Returns \
             {\"drained\": false} if the queue was empty. This call BLOCKS until the task \
             completes — it is not fire-and-forget and should not be used on a latency-sensitive path.",
            json_schema(json!({"type":"object","properties":{}})),
        ),
        Tool::new(
            "context",
            "Evidence-driven context assembly for a natural-language engineering question. \
             Classifies the query, applies the Query Evidence Contract for its intent \
             (definition/navigation/configuration/architecture/debugging/impact/dependency/\
             test/knowledge), retrieves candidates from lexical+symbol+structural+relationship+\
             knowledge indexes, validates freshness/provenance/confidence, expands bounded \
             (graph walk or secure source verification) when requirements are unmet, and \
             returns a secret-free, provenance-stamped context with verified claims — or an \
             explicit INSUFFICIENT_EVIDENCE verdict. Modes: FAST (index-only), NORMAL, DEEP.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "query":         {"type":"string","description":"Natural-language question (max 512 chars)"},
                    "mode":          {"type":"string","enum":["FAST","NORMAL","DEEP"],"description":"Answer-mode policy (default NORMAL)"},
                    "repository_id": {"type":"string","description":"Optional repository UUID scope"}
                },
                "required": ["query"]
            })),
        ),
        Tool::new(
            "workspace",
            "Inspect and manage the configured logical workspace membership at runtime. \
             Actions: `inspect` (report the configured + active roots and per-repository \
             state), `add <path>`, `remove <path>`, `set [<paths...>]` (authoritatively \
             replace membership). The configuration is persisted atomically to \
             <ATTIC_HOME>/config.toml so it survives restarts; membership changes take \
             effect live (bootstrap/index for newly added roots, watcher stop for removed \
             ones). On a pristine machine this is the first-run configuration entry point.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "action": {"type":"string","enum":["inspect","add","remove","set"],"description":"Membership operation"},
                    "path":  {"type":"string","description":"Filesystem path for add/remove"},
                    "paths": {"type":"array","items":{"type":"string"},"description":"Full membership for set"}
                },
                "required": ["action"]
            })),
        ),
    ]
}
