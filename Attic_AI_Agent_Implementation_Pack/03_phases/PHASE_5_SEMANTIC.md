# Phase 5 — Semantic Intelligence

## Entry condition
Phase 4 non-semantic benchmark exists.

## Rule
Do not embed all retrieval units by default.

## Sequence
1. define semantic-provider interface;
2. define semantic-unit selection;
3. add embeddings for selected units;
4. benchmark;
5. add hybrid candidate generation;
6. benchmark;
7. add reranking only if needed;
8. benchmark;
9. summaries only if justified.

## Disposable layer invariant
Changing/removing the embedding model must not invalidate source, FTS, structural data, or canonical evidence lineage.

## Failure behavior
Semantic provider unavailable:
- FAST unaffected;
- NORMAL follows contract;
- DEEP may degrade with explicit diagnostic;
- no false completeness.

## Gate
Retain each semantic feature only if measured quality gain justifies latency/resource/complexity cost.
