Remaining optional follow-up work (none blocks the core acceptance condition, which passes):

1. Add focused §37 failure-case tests: permission-denied root, config write failure, corrupted config parse behavior, watcher startup failure for one member, unavailable-member later recovery (§35).
2. Add §35 restart tests: reordered config preserves repository identities; configure A/B/C with B unavailable → A/C usable, B reported degraded.
3. Optionally persist a "pending/degraded" marker for adds whose indexing failed after config persistence (§23 full semantics).
4. Run the focused configuration/multi-root/MCP test suites once more after any changes.
5. No git writes — leave everything uncommitted for review.