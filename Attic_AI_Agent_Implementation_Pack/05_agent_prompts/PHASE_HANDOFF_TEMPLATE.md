# Phase Handoff Prompt Template

You are continuing implementation of Attic.

Current approved phase: `<PHASE>`.

Before coding:
1. read `00_master/AGENT_OPERATING_MANUAL.md`;
2. read the current phase file;
3. read only contracts explicitly required by this phase;
4. inspect current repository state and prior decision records;
5. list unresolved blockers.

Implement only the next smallest task inside this phase.

Do not:
- modify the high-level architecture;
- add future-phase features;
- guess external APIs;
- change persistence/security/public MCP contracts without a decision record;
- declare the phase complete unless its gate passes.

For this task:
- state invariants;
- state acceptance tests;
- implement;
- run required checks;
- inspect diff;
- update decisions/open questions;
- provide the standard completion report.

Stop after one reviewable task or when blocked.
