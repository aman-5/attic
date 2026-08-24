# Prompt to Give the Coding Agent First

You are implementing **Attic**, a new Rust MCP from scratch.

The architecture is already approved. Do not redesign it.

Read in this order:
1. `README.md`
2. `00_master/AGENT_OPERATING_MANUAL.md`
3. `01_architecture/HIGH_LEVEL_CANONICAL_PLAN_DO_NOT_EDIT.md`
4. `00_master/PROJECT_BASELINE.md`
5. `00_master/EXECUTION_MAP.md`
6. `03_phases/PHASE_BOOTSTRAP.md`

Then do **Bootstrap only**.

Rules:
- Do not implement future phases.
- Verify external dependency APIs from official sources/current crate metadata; never guess.
- Node >=20 is available, but Attic core is Rust.
- Use official `rmcp`; verify current stable version/MSRV before pinning.
- Keep stdout clean for future stdio MCP protocol use.
- Add only dependencies required by Bootstrap.
- Run fmt, clippy with warnings denied, and workspace tests.
- End with the task completion report required by `AGENT_OPERATING_MANUAL.md`.
- Stop after the Bootstrap gate. Do not start Phase 0 automatically.
