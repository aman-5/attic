# Phase 4 — Evidence-Driven Retrieval

## Goal
Implement the core quality architecture before embeddings.

## Required objects
- QueryType
- QueryEvidenceContract
- AnswerModePolicy
- RetrievalPlan
- Candidate
- Evidence
- ValidatedEvidence
- Claim

## Pipeline
```text
question
→ query router
→ answer mode
→ evidence contract
→ retrieval planner
→ RetrievalPlan
→ candidate generators
→ fusion
→ evidence ranking
→ evidence validation
→ sufficiency
→ targeted expansion/source verification if needed
→ context
→ answer
→ claim/evidence verification
```

## FAST/NORMAL/DEEP
Budgets come from configuration/contracts. No unbounded traversal/read.

## Ranking vs validation
Ranking asks "likely useful?"
Validation asks "can it support the requirement?"

Never merge them into one opaque score.

## Direct source verification
Use when contract requires current exact source, evidence is stale/dirty/contradictory, or indexed evidence is insufficient.

## Insufficient evidence
Return explicit internal state; do not force a fabricated answer.

## Answer verification
Deterministic-first:
- claims map to evidence IDs;
- spans support claims;
- revision/freshness consistent;
- relationship claims meet resolution/confidence;
- contradictions surfaced.

## Gate
Run full non-semantic benchmark. Phase 5 is forbidden until results are recorded.
