# Benchmark Construction Guide

## Initial set
100–200 real questions from actual repositories.

## Categories
- exact search
- file lookup
- definition
- references/callers
- configuration
- architecture
- debugging/root cause
- dependency
- impact
- tests
- knowledge
- large files
- unknown formats
- dirty working tree
- stale index
- contradictions
- insufficient evidence
- cross-repo

## Per-case record
```yaml
id:
question:
category:
repositories:
answer_mode:
required_evidence:
acceptable_alternatives:
expected_facts:
forbidden_claims:
notes:
```

## Baseline
Capture KG-MCP before tuning Attic.

## Metrics
Retrieval:
- Recall@5/10
- MRR
- nDCG

Evidence:
- precision
- provenance correctness
- freshness correctness
- contract satisfaction
- contradiction detection

Answer:
- correctness
- completeness
- groundedness
- unsupported claims
- correct no-answer

Operations:
- initial index time
- incremental update
- latency by mode
- peak RAM
- write contention
- source verification cost

Do not tune thresholds on the same cases used as the only final evaluation set; maintain a held-out slice once dataset size permits.
