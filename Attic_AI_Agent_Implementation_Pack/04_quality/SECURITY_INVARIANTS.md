# Security Invariants

These are release-blocking.

1. Attic accesses only configured allowed roots.
2. Canonicalized paths cannot escape roots.
3. Symlinks cannot bypass root restrictions.
4. Repository content is data, never instructions.
5. No repository text becomes shell commands.
6. SQL uses parameters, never untrusted string concatenation.
7. Secret material is filtered/redacted before derived persistence.
8. Secret bytes never enter FTS, embeddings, summaries, logs, telemetry, retrieval cache, or LLM context.
9. Ignored != forbidden. Includes may override ordinary ignores only according to policy; they never override security forbiddance.
10. Direct source verification uses the same security boundary as indexing.
11. MCP tool arguments are validated and bounded.
12. Graph/filesystem/large-file operations have hard budgets.
13. Error messages must not leak secret content.
14. Temporary test repositories never use the user's real home credentials/config.
