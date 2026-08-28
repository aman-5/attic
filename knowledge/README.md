# Project Knowledge

This directory is **curated project knowledge** — a small set of Markdown
files that a maintainer deliberately writes to record durable facts about
this project that aren't obvious from reading the code line-by-line: why the
architecture is shaped the way it is, what the domain vocabulary means, team
conventions, ownership, deployment topology.

Attic treats everything under `knowledge/**/*.md` as a distinct evidence
tier — `EvidenceSourceType::Knowledge`, mapped to `AuthorityLevel::
ProjectKnowledge` — the highest documentation authority Attic assigns.
**Nothing outside this directory receives that authority**, no matter what
it's named: an ordinary `README.md` or `docs/ARCHITECTURE.md` anywhere else
in the repository remains ordinary `Documentation`-tier evidence — real,
searchable, useful, but not treated as deliberately-curated project
knowledge. This is a hard path-based boundary
(`crates/attic-retrieval/src/candidates.rs::source_type_for_path`), not a
filename heuristic.

## What belongs here

Durable, slow-changing facts a new engineer or an AI assistant would
otherwise have to reconstruct by reading a lot of code:

- **`architecture.md`** — why the system is shaped this way, not just what
  it does (that belongs in your regular `docs/ARCHITECTURE.md`, if you have
  one — this file is for the *rationale* your code and generated docs can't
  carry on their own).
- **`domain.md`** — domain vocabulary, business rules, terms that mean
  something specific in this codebase.
- **`conventions.md`** — team conventions not enforced by a linter (naming,
  module layout, review expectations).
- **`ownership.md`** — who/which team owns which area, for routing
  questions.
- **`deployment.md`** — how and where this actually runs in practice.

These filenames are examples, not a requirement — `knowledge/README.md`
(this file) is the only thing Attic expects; everything else under
`knowledge/` is up to you.

## What does not belong here

- **Secrets, credentials, tokens, internal URLs you wouldn't want in search
  results.** Knowledge files are indexed and served through the same `file`/
  `search`/`context` tools as source code — Attic's secrets-scanning layer
  still applies, but the best mitigation is to never write secrets into
  Markdown in the first place.
- **Temporary chat instructions or session-scoped notes.** Knowledge files
  should describe durable project facts, not "for this task, do X" —
  instructions like that belong in your AI tool's own prompt/config, not in
  a file Attic indexes as permanent project authority.
- **Anything you want machine-executed.** These are curated *documentation*,
  not executable instructions — Attic (and any AI assistant reading its
  output) treats them as claims to weigh, never as commands to run.

## How Attic treats this content

- Curated knowledge is **evidence with high authority, not ground truth**.
  When a `context` query assembles an answer, a knowledge claim can outweigh
  other documentation, but source code and passing tests remain the
  strongest evidence for what the system actually does *right now*.
- **Attic preserves contradiction rather than silently trusting stale
  knowledge.** If `knowledge/architecture.md` says one thing and the actual
  source/config says another, Attic surfaces both with their respective
  authority and freshness rather than picking one and hiding the conflict.
  Keep these files current, but a stale one degrades gracefully — it does
  not corrupt or override retrieval.
- Knowledge files are indexed and kept fresh exactly like source: edit one,
  and Attic's incremental watcher reindexes it like any other tracked file.

Project knowledge is entirely **optional** — a repository with no
`knowledge/` directory at all works exactly the same as one with an empty
one; nothing degrades, and there is no `ProjectKnowledge`-tier evidence to
draw on.
