# Phase 6 — Cross-Repository Intelligence

## Goal
Resolve and traverse relationships across repositories safely.

## Build dependency basis first
Examples:
- Maven/Gradle
- Go modules
- npm packages/workspaces
- Python packages
- Git submodules
- generated APIs
- configuration references

## Do not infer equality from names
A string/symbol name match across repos is not a resolved dependency.

## Graph budgets
Every traversal obeys:
- depth;
- node count;
- edge count;
- time;
- context/evidence budget.

## Impact analysis
Must distinguish:
- direct resolved dependents;
- indirect resolved dependents;
- inferred/possible impacts.

## Gate
Cross-repo benchmark passes without graph explosion or confidence laundering.
