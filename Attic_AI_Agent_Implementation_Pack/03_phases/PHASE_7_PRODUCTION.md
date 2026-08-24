# Phase 7 — Production Hardening

## Areas

### Resource management
- bounded queues;
- memory-aware admission;
- worker concurrency;
- isolated heavy processes where justified;
- disk I/O limits;
- cancellation/deadlines.

### Resilience
- crash/restart;
- DB recovery;
- interrupted migrations;
- disk-full behavior;
- corrupted operational state;
- stale semantic layer;
- repository disappearance.

### Observability
Trace query:
```text
query
→ RetrievalPlan
→ candidates
→ ranking signals
→ accepted/rejected evidence
→ source verification
→ context
→ claims
→ verification
```

### Packaging
Support Linux/macOS. Document installation, workspace config, upgrade/migration, troubleshooting.

### Security
Run full secret/path/symlink/untrusted-content test suite.

### Performance
Benchmark explicit hardware profiles.

## Gate
Only here may documentation call Attic production-ready.
