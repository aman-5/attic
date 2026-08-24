# Phase 1D — Minimum Useful MCP + FTS

## Goal
Expose a small, correct MCP backed by lexical/structural evidence.

## Transport
Start with stdio unless requirements explicitly demand another transport.

With stdio:
- stdout is protocol-only;
- diagnostics go to stderr/tracing.

## MCP SDK
Use official `rmcp`. Verify exact 3.x API from official docs/source at implementation time; do not copy old 0.x/1.x examples blindly.

## Initial tools
Implement only:
- `search`
- `file`
- `repo_map`
- `status`

Do not expose internal DB/query/analyzer primitives.

## Search
V1 search:
- FTS5;
- exact/path lookup;
- repository filters;
- type/language filters where contracted;
- provenance in results.

## File
Reads bounded source regions after security/path validation.

## Status
Expose:
- repositories;
- source revision/index state;
- indexing progress;
- diagnostics at safe granularity.

## Integration tests
Launch Attic as child process and use an MCP client/test harness to:
- initialize/discover according to verified SDK/spec behavior;
- list/call tools;
- verify schemas;
- verify malformed inputs;
- ensure logs do not corrupt stdio.

## Gate
A clean fixture workspace can be indexed and queried through MCP with grounded file/path/span results.
