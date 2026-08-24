# Phase 1B — Git-Aware Discovery and Security

## Goal
Produce the exact eligible-file set safely and reproducibly.

## Pipeline

```text
configured workspace roots
→ canonicalize
→ security allowlist
→ repository detection
→ Git-aware ignore evaluation
→ Attic include/exclude policy
→ default exclusions
→ file classification
→ eligible manifest
```

## Requirements

### Git repositories
Respect:
- tracked files;
- eligible untracked files;
- nested `.gitignore`;
- negation;
- `.git/info/exclude`.

### Defaults
Normally ignore:
`.git`, `node_modules`, build/dist/target output, coverage, caches, virtualenvs.

Do not universally ignore tests, generated source, vendor, fixtures, snapshots.

### Security
Symlink/path traversal must not escape allowed roots.

### Secret preprocessing
Before searchable persistence, apply secret contract.

### SourceRevision
Build deterministic eligible manifest. If source mutates during capture, use bounded retry/unstable-state behavior from contract.

## Fixtures
Create temporary Git repos with:
- nested ignores;
- negation;
- untracked files;
- rename;
- symlink escape;
- build folders;
- tests;
- generated folder;
- dirty file;
- changing ignore file.

## Gate
Given the same repository state/policy, discovery produces the same manifest. Forbidden/ignored content never reaches indexing input.
