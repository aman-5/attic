use rmcp::model::Tool;
use serde_json::{Value, json};

// ─── schema helper ─────────────────────────────────────────────────────────────

fn json_schema(v: Value) -> std::sync::Arc<serde_json::Map<String, Value>> {
    std::sync::Arc::new(v.as_object().cloned().unwrap_or_default())
}

/// Freshness-gate arguments shared by every index-reading tool.
fn with_freshness_args(mut v: Value) -> Value {
    if let Some(props) = v["properties"].as_object_mut() {
        props.insert(
            "version_token".into(),
            json!({"type":"string","description":"Pin this call to the code version of an earlier call in the same answer (freshness.version_token). If the code changed since, the call fails with VERSION_EXPIRED: restart the answer without a token"}),
        );
        props.insert(
            "expected_branch".into(),
            json!({"type":"string","description":"Refuse with BRANCH_MISMATCH unless this Git branch is checked out (e.g. the production branch you are investigating)"}),
        );
        props.insert(
            "expected_commit".into(),
            json!({"type":"string","description":"Refuse with BRANCH_MISMATCH unless HEAD is this commit (full SHA or 7+ char prefix)"}),
        );
        props.insert(
            "deep_verify".into(),
            json!({"type":"boolean","description":"Hash every file instead of trusting unchanged size/timestamps/file id (slower; for network or cloud-sync folders)"}),
        );
        props.insert(
            "at_commit".into(),
            json!({
                "description":"Answer from a named Git commit (e.g. the deployed production SHA or tag) instead of the working tree, without checking it out. A string (with repository_id/repository) or {repository_id: commit} for several repositories (context only; cross-repository links are resolved for exactly those commits).",
                "oneOf":[{"type":"string"},{"type":"object","additionalProperties":{"type":"string"}}]
            }),
        );
    }
    v
}

/// Shared tail of index-reading tool descriptions.
const FRESHNESS_NOTE: &str = " FRESHNESS: before reading, Attic checks the saved files of every \
     repository it reads (Git or plain folder) and re-indexes anything that changed; the \
     response's `freshness` block names the code (branch/commit or folder fingerprint) and \
     carries `version_token`. Errors are JSON {code, retryable}: SOURCE_CHANGING, \
     SOURCE_UNAVAILABLE, INDEX_NOT_READY, TIMEOUT (retry), VERSION_EXPIRED (restart the \
     answer), BRANCH_MISMATCH.";

// ─── build the tool list once ──────────────────────────────────────────────────

pub(crate) fn make_tools() -> Vec<Tool> {
    let mut tools = make_base_tools();
    for tool in &mut tools {
        if GATED_TOOLS.contains(&tool.name.as_ref()) {
            let schema = with_freshness_args(Value::Object((*tool.input_schema).clone()));
            tool.input_schema = json_schema(schema);
            let description = format!(
                "{}{FRESHNESS_NOTE}",
                tool.description.as_deref().unwrap_or_default()
            );
            tool.description = Some(description.into());
        }
    }
    tools
}

/// Tools that read index data and therefore pass the freshness gate (R1).
pub(crate) const GATED_TOOLS: &[&str] = &["file", "repo_map", "context"];

/// Tools that never read index data, with the reason they skip the gate.
#[cfg(test)]
pub(crate) const UNGATED_TOOLS: &[(&str, &str)] = &[
    ("egm", "reads planning configuration only"),
    ("status", "reports server/index health, not code"),
    ("logging", "controls log files"),
    ("debug_drain_task", "admin: runs background indexing work"),
    ("workspace", "manages membership"),
    ("spec", "spec documents live in their own store"),
];

fn make_base_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "egm",
            "Read EGM activation status, user enable/disable commands, or mode-aware reviewer \
             workflow guidance. Available even when \
             EGM is disabled. Configuration changes are user CLI operations; this tool never \
             enables enforcement, installs reviewers/hooks, or approves a spec.",
            json_schema(json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "action": {"type":"string","enum":["status","guidance"],"default":"status"}
                }
            })),
        ),
        Tool::new(
            "file",
            "Read live, authoritative source from an indexed repository through the \
             secrets-scan layer (redacted/excluded files are flagged). Pick ONE selector: \
             (1) path + optional start_line/end_line (1-indexed, inclusive) or \
             start_byte/end_byte (0-indexed, exclusive; overrides lines; floored to UTF-8 \
             boundaries); (2) ranges: up to 16 {path,start_line?,end_line?} reads in ONE call — \
             prefer this over repeated calls after SEARCH; output is capped at 128 KiB and \
             ranges that do not fit are listed as omitted, never cut; (3) unit_id from a \
             SEARCH result (indexed location, may be stale); (4) JSON navigation: \
             json_pointer, parent, outline/outline_depth; (5) grep (+regex, max_matches) for \
             literal/regex matches with coverage metadata. Large files are streamed; output \
             is capped server-side.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "repository":    {"type":"string","description":"Unambiguous active repository name, UUID, or absolute root path"},
                    "path":          {"type":"string","description":"Repo-relative path"},
                    "ranges":        {
                        "type":"array","minItems":1,"maxItems":16,
                        "description":"Batch read: several line ranges in one call; excludes every other selector",
                        "items": {
                            "type":"object","additionalProperties":false,"required":["path"],
                            "properties": {
                                "path":       {"type":"string"},
                                "start_line": {"type":"integer","minimum":1},
                                "end_line":   {"type":"integer","minimum":1}
                            }
                        }
                    },
                    "unit_id":       {"type":"string","description":"Indexed retrieval unit UUID (retrieval_unit_id from SEARCH); may replace path"},
                    "json_pointer":  {"type":"string","description":"JSON only: RFC 6901 pointer into the live file"},
                    "parent":        {"type":"boolean","description":"JSON only: expand the parent of the selected value"},
                    "outline":       {"type":"boolean","description":"JSON only: bounded structural outline"},
                    "outline_depth": {"type":"integer","minimum":1,"maximum":5,"description":"JSON outline only: depth (default 2)"},
                    "grep":          {"type":"string","description":"Find literal text in this file"},
                    "regex":         {"type":"boolean","description":"grep only: treat grep as a regular expression"},
                    "max_matches":   {"type":"integer","minimum":1,"maximum":200,"description":"grep only: maximum matching lines (default 50)"},
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
            "Return a bounded, paginated structure map and statistics for an indexed repository. \
             Use next_offset to continue; narrow with path_prefix or file_type.",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "repository_id": {"type":"string","description":"UUID of the repository"},
                    "repository":    {"type":"string","description":"Unambiguous active repository name, UUID, or absolute root path"},
                    "file_type":     {"type":"string","description":"Optional file-type filter"},
                    "path_prefix":   {"type":"string","description":"Repo-relative directory prefix, matched at path-component boundaries"},
                    "offset":        {"type":"integer","minimum":0,"description":"File offset from next_offset (default 0)"},
                    "limit":         {"type":"integer","minimum":1,"maximum":1000,"default":200},
                    "max_depth":     {"type":"integer","minimum":1,"maximum":32,"default":4},
                    "max_bytes":     {"type":"integer","minimum":4096,"maximum":131072,"default":32768}
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
            "One door for code retrieval. MODES: \
             NORMAL (default) — evidence-driven answer to a natural-language engineering \
             question: classifies intent, retrieves lexical+symbol+structural+relationship+\
             knowledge candidates, verifies freshness/provenance, expands bounded when \
             evidence is short, returns secret-free cited context and verified claims, or \
             INSUFFICIENT_EVIDENCE. FAST — index-only, cheapest. DEEP — up to 30 s; some \
             clients time out, so escalate from NORMAL only on INSUFFICIENT_EVIDENCE. \
             SEARCH — raw lexical (SQLite FTS5) identifier/string lookup, no evidence \
             assembly; each hit has source_type, a ~240-char snippet and line anchors; \
             read several hits at once with file(ranges=[...]). Plain terms are ranked OR; \
             quoted phrases and AND/OR/NOT groups work. REFERENCES — bounded literal \
             reference scan with explicit coverage and continuation fields. \
             ALL MODES: repository/repository_id, one of path/path_prefix/path_glob (applied \
             before ranking), max_bytes (lower the response cap: FAST 64 KiB, NORMAL/SEARCH/\
             REFERENCES 128 KiB, DEEP 256 KiB). Answer modes may add central knowledge and \
             workspace relationship evidence unless a path scope is set; identical excerpts \
             share one body; repository_answers holds summaries only. Budgets keep whole \
             evidence and report omissions in response_budget. A hard-cancelled query \
             returns validated evidence with POLICY_HARD_CANCELLED/NONE. telemetry reports \
             server timings, response bytes and approx_tokens (bytes/4 estimate).",
            json_schema(json!({
                "type": "object",
                "properties": {
                    "query":         {"type":"string","description":"Question or lookup text (max 512 chars)"},
                    "mode":          {"type":"string","enum":["FAST","NORMAL","DEEP","SEARCH","REFERENCES"],"description":"Default NORMAL"},
                    "repository_id": {"type":"string","description":"Optional repository UUID scope"},
                    "repository":    {"type":"string","description":"Optional unambiguous active repository name, UUID, or absolute root path"},
                    "path":          {"type":"string","description":"Exact repository-relative file path"},
                    "path_prefix":   {"type":"string","description":"Repository-relative directory and descendants"},
                    "path_glob":     {"type":"string","description":"Repository-relative path glob using * and ?"},
                    "max_bytes":     {"type":"integer","minimum":8192,"maximum":262144,"description":"Lower the response cap for this call; never raises the mode cap"},
                    "include_diagnostics": {"type":"boolean","default":false,"description":"Answer modes only: list individual rejected evidence (counts/reasons always returned)"},
                    "match_mode":    {"type":"string","enum":["any","all"],"description":"SEARCH only: ranked OR (default) or strict AND of adjacent terms"},
                    "file_type":     {"type":"string","description":"SEARCH only: file extension filter (max 32)"},
                    "language":      {"type":"string","description":"SEARCH only: detected-language filter (max 64)"},
                    "scope":         {"type":"string","enum":["all","knowledge"],"description":"SEARCH only: \"knowledge\" = project knowledge only (central folder first, then repository knowledge/ folders). Default \"all\""},
                    "max_results":   {"type":"integer","minimum":1,"maximum":200,"description":"SEARCH (default 25) and REFERENCES (default 100) only"},
                    "max_files":     {"type":"integer","minimum":1,"maximum":64,"description":"REFERENCES only: maximum files to scan (default 32)"},
                    "after_path":    {"type":"string","description":"REFERENCES only: continue after this path within one repository"},
                    "reference_offset": {"type":"integer","minimum":0,"description":"REFERENCES only: resume JSON matches in an exact file"},
                    "reference_start_line": {"type":"integer","minimum":1,"description":"REFERENCES only: resume text matches in an exact file"}
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

#[cfg(test)]
mod freshness_coverage_tests {
    use super::*;

    /// R1 guard: every registered tool is classified exactly once as gated
    /// (reads index data through the freshness gate) or exempt with a
    /// reason, so a new tool cannot silently skip the gate.
    #[test]
    fn every_tool_is_classified_for_the_freshness_gate() {
        let mut names: Vec<String> = make_tools().iter().map(|t| t.name.to_string()).collect();
        names.push(crate::spec_tool::tool().name.to_string());
        for name in &names {
            let gated = GATED_TOOLS.contains(&name.as_str());
            let exempt = UNGATED_TOOLS.iter().any(|(n, _)| n == name);
            assert!(
                gated ^ exempt,
                "tool `{name}` must be listed in exactly one of GATED_TOOLS / UNGATED_TOOLS"
            );
        }
        for tool in make_tools() {
            let has_token = tool.input_schema["properties"]
                .get("version_token")
                .is_some();
            assert_eq!(
                has_token,
                GATED_TOOLS.contains(&tool.name.as_ref()),
                "{}",
                tool.name
            );
        }
    }
}
