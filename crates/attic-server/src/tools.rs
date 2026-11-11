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
             JSON pointers, indexed unit IDs, parent expansion, and bounded JSON outlines \
             navigate the current sanitized file. Unit locations are indexed and may be stale. \
             grep supports literal or regex matching in sanitized lines, with bounded \
             per-line byte columns, explicit coverage, and continuation metadata. \
             LARGE files are streamed; responses are capped by server-side output limits.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "repository":    {"type":"string","description":"Unambiguous active repository name, UUID, or absolute root path"},
                    "path":          {"type":"string","description":"Repo-relative path"},
                    "unit_id":       {"type":"string","description":"Indexed retrieval unit UUID; may replace path"},
                    "retrieval_unit_id": {"type":"string","description":"Indexed retrieval unit UUID; may replace path (alias: unit_id)"},
                    "json_pointer":  {"type":"string","description":"RFC 6901 pointer into the live JSON file"},
                    "parent":        {"type":"boolean","description":"Expand the parent of the selected JSON value"},
                    "outline":       {"type":"boolean","description":"Return a bounded JSON structural outline"},
                    "outline_depth": {"type":"integer","minimum":1,"maximum":5,"description":"Outline depth (default 2)"},
                    "grep":          {"type":"string","description":"Find literal text in this file"},
                    "regex":         {"type":"boolean","description":"Interpret grep as a regular expression"},
                    "max_matches":   {"type":"integer","minimum":1,"maximum":200,"description":"Maximum matching lines (default 50)"},
                    "start_line":    {"type":"integer","description":"First line (1-indexed)"},
                    "end_line":      {"type":"integer","description":"Last line (1-indexed, inclusive)"},
                    "start_byte":    {"type":"integer","description":"Start byte offset (0-indexed, overrides lines)"},
                    "end_byte":      {"type":"integer","description":"End byte offset (0-indexed, exclusive)"}
                },
                "anyOf": [{"required":["repository_id"]},{"required":["repository"]}]
            })),
        ),
        Tool::new(
            "repo_map",
            "Return statistics and structure map for an indexed repository.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "repository":    {"type":"string","description":"Unambiguous active repository name, UUID, or absolute root path"},
                    "file_type":     {"type":"string","description":"Optional file-type filter"}
                },
                "anyOf": [{"required":["repository_id"]},{"required":["repository"]}]
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
            "One door for retrieval and answers. Default (NORMAL): evidence-driven context \
             assembly for a natural-language engineering question — classifies the query, \
             applies the Query Evidence Contract for its intent (definition/navigation/\
             configuration/architecture/debugging/impact/dependency/test/knowledge), \
             retrieves candidates from lexical+symbol+structural+relationship+knowledge \
             indexes, validates freshness/provenance/confidence, expands bounded (graph \
             walk or secure source verification) when requirements are unmet, and returns \
             a secret-free, provenance-stamped context with verified claims — or an \
             explicit INSUFFICIENT_EVIDENCE verdict. Modes: FAST (index-only, cheapest), \
             NORMAL (default), DEEP (up to 30 s of work; some MCP clients time out long \
             requests — prefer NORMAL and escalate to DEEP on INSUFFICIENT_EVIDENCE). \
             A hard-cancelled query still returns whatever evidence completed validation, \
             with result POLICY_HARD_CANCELLED and confidence NONE. \
             mode=\"SEARCH\": raw lexical retrieval (SQLite FTS5) \
             WITHOUT evidence assembly — for exact-string/identifier lookup, not \
             questions. Each SEARCH result carries a source_type, a bounded ~240-char \
             snippet, and start/end line anchors when recorded — use the `file` tool for \
             full content. SEARCH accepts file_type, language, max_results (default 25, \
             cap 200), and scope (\"all\"|\"knowledge\"). SEARCH combines plain terms \
             with ranked OR by default; quoted phrases and explicit AND/OR/NOT groups \
             are supported. Set match_mode=\"all\" for strict adjacent terms. \
             path, path_prefix, and path_glob restrict results before ranking. \
             Answer modes may include central knowledge and workspace relationship \
             evidence when only a repository is selected; an explicit path scope \
             disables those additions. \
             mode=\"REFERENCES\" scans bounded sanitized indexed files for literal references, \
             distinguishes syntactic JSON keys/string matches and reports incomplete coverage. \
             Answer modes share identical excerpt bodies while retaining every citation; \
             repository_answers contains summaries, not duplicate full answers. \
             Rejected-evidence counts/reasons are always included; set include_diagnostics=true \
             for individual rejected items. Response budgets preserve whole evidence and \
             report omitted results; narrow repository/path scope for more. telemetry reports \
             server timings and UTF-8 response bytes, not client/model processing time.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "query":         {"type":"string","description":"Natural-language question (max 512 chars); SEARCH supports quoted phrases and AND/OR/NOT"},
                    "mode":          {"type":"string","enum":["FAST","NORMAL","DEEP","SEARCH","REFERENCES"],"description":"Answer-mode policy (default NORMAL); SEARCH = raw lexical retrieval; REFERENCES = bounded syntactic reference scan"},
                    "repository_id": {"type":"string","description":"Optional repository UUID scope"},
                    "repository":    {"type":"string","description":"Optional unambiguous active repository name, UUID, or absolute root path"},
                    "path":          {"type":"string","description":"Exact repository-relative file path"},
                    "path_prefix":   {"type":"string","description":"Repository-relative directory and descendants"},
                    "path_glob":     {"type":"string","description":"Repository-relative path glob using * and ?"},
                    "match_mode":    {"type":"string","enum":["any","all"],"description":"SEARCH: combine adjacent terms with ranked OR (default) or strict AND"},
                    "file_type":     {"type":"string","description":"SEARCH mode: filter by file extension (max 32)"},
                    "language":      {"type":"string","description":"SEARCH mode: filter by detected language (max 64)"},
                    "max_results":   {"type":"integer","minimum":1,"maximum":200,"description":"SEARCH default 25; REFERENCES default 100; maximum 200"},
                    "max_files":     {"type":"integer","minimum":1,"maximum":64,"description":"REFERENCES: maximum files to scan (default 32)"},
                    "after_path":    {"type":"string","description":"REFERENCES: continue after this path within one repository"},
                    "reference_offset": {"type":"integer","minimum":0,"description":"REFERENCES: resume JSON matches in an exact file"},
                    "reference_start_line": {"type":"integer","minimum":1,"description":"REFERENCES: resume text matches in an exact file"},
                    "scope":         {"type":"string","enum":["all","knowledge"],"description":"SEARCH mode: \"knowledge\" = only project knowledge (central knowledge folder first, then repository knowledge/ folders). Default \"all\""}
                    ,"include_diagnostics": {"type":"boolean","default":false,"description":"Answer modes: include individual rejected-evidence diagnostics; counts/reasons are always returned"}
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
             <ATTIC_HOME>/config/config.toml in the structured home layout, so it survives \
             restarts; membership changes take \
             effect live (bootstrap/index for newly added roots, watcher stop for removed \
             ones). Up to 65 active repository stores are supported. On a pristine \
             machine this is the first-run configuration entry point.",
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
