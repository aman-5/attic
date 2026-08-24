# Independent Review Prompt

Review the current Attic implementation against the approved phase and contracts.

Do not propose new architecture unless a concrete correctness, security, scalability, or benchmark failure requires it.

Check:
- phase leakage;
- invented/unused dependencies;
- contract violations;
- unsafe unwrap/expect;
- missing error context;
- secret leakage;
- path/symlink bypass;
- SQLite transaction incoherence;
- stale/ghost evidence;
- missing provenance;
- nondeterministic tests;
- unbounded graph/file operations;
- MCP stdout contamination;
- public tool-schema drift;
- insufficient tests.

For every finding provide:
```text
severity
contract/phase violated
file/location
failure scenario
minimal correction
test that should catch it
```

Do not mark code correct merely because it compiles.
