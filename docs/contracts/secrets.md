# Contract: Secret Scanning and Non-Persistence

## Purpose

Define how Attic detects, classifies, and handles secret material in repository
content so that secret bytes never enter any derived persistence layer.

This contract is a security invariant. It cannot be weakened by configuration
or operator override.

---

## Definitions

### SecretState

The security classification assigned to each file or content fragment.

```
SAFE               -- no detected secret material
PARTIALLY_REDACTED -- some regions redacted; remaining content may be indexed
SECRET_REDACTED    -- entire file treated as secret; no content indexed
FORBIDDEN          -- path-level exclusion (see discovery contract); not read
```

### RedactedSpan

A byte range within a file's content that has been identified as containing
secret material and replaced with a safe placeholder.

```
RedactedSpan {
  start_byte : u64
  end_byte   : u64
  secret_kind: String   -- e.g., "API_KEY", "JWT", "PRIVATE_KEY"
  placeholder: String   -- replacement text, e.g., "[REDACTED:API_KEY]"
}
```

---

## Secret Scanning Point

Secret scanning happens at the file pipeline stage, BEFORE any content reaches:

```
FTS index
embeddings
summaries
logs
telemetry
retrieval caches
LLM context
answer context
ops_* tables
core_evidence.ranking_signals_json
```

The pipeline order is:

```
File content bytes
    |
    v
[1] Encoding detection
    |
    v
[2] Binary detection → if binary: skip scanning, SAFE (binary has no text secrets)
    |
    v
[3] Secret scanner
    |
    +-- No secrets found → SecretState::SAFE → proceed to analysis
    |
    +-- Partial secrets (redactable) → SecretState::PARTIALLY_REDACTED
    |       → replace RedactedSpans with placeholders
    |       → redacted content proceeds to analysis
    |
    +-- Whole-file secret (e.g., private key file) → SecretState::SECRET_REDACTED
            → no content enters any derived layer
            → FileOccurrence.existence_state = SECRET_REDACTED
            → FileOccurrence records path and content_hash only
```

---

## What Must Not Be Persisted

The following MUST NOT contain secret bytes under any circumstances:

| System | Notes |
|--------|-------|
| `core_retrieval_units.retrieval_text` | FTS source; never contains secret content |
| `fts_retrieval_units` | FTS virtual table |
| `fts_symbol_names` | FTS virtual table |
| `core_structural_nodes.metadata_json` | Analyzer output |
| `core_evidence` (any column) | Evidence presented to LLM |
| `core_knowledge_items` content | Knowledge doc content |
| Semantic vector embeddings | May encode secret content if present |
| `ops_*` log columns | Error messages, task checkpoints |
| Application logs (stdout/stderr) | Structured and unstructured logs |
| Telemetry | Any external observability data |
| `core_relationships.provenance_json` | Must not quote secret source lines |

What IS persisted for SECRET_REDACTED files:

```
FileOccurrence.path                  -- path is not secret
FileOccurrence.content_hash          -- hash is not secret (no preimage attack)
FileOccurrence.size_bytes            -- size is not secret
FileOccurrence.existence_state       -- SECRET_REDACTED
FileOccurrence.security_state        -- SECRET_REDACTED
```

---

## Detected Secret Types (V1 Baseline)

The following patterns trigger secret classification:

```
Private key blocks
  -----BEGIN RSA PRIVATE KEY-----
  -----BEGIN EC PRIVATE KEY-----
  -----BEGIN PRIVATE KEY-----
  -----BEGIN OPENSSH PRIVATE KEY-----
  -----BEGIN PGP PRIVATE KEY BLOCK-----

Generic high-entropy strings
  Continuous strings of >= 20 characters with entropy > 4.5 bits/char
  in assignment context (e.g., `key = "..."`, `secret = "..."`, `token = "..."`)

Common API key patterns
  AWS:      AKIA[0-9A-Z]{16}
  GitHub:   ghp_[A-Za-z0-9]{36}  |  github_pat_[A-Za-z0-9_]{82}
  Stripe:   sk_live_[0-9a-zA-Z]{24,}
  Slack:    xoxb-[0-9]{11}-[0-9]{11}-[A-Za-z0-9]{24}
  Twilio:   SK[0-9a-fA-F]{32}
  Generic bearer token in assignment: bearer [A-Za-z0-9\-._~+/]{20,}

JWT tokens
  eyJ[A-Za-z0-9-_]+\.[A-Za-z0-9-_]+\.[A-Za-z0-9-_]+

Connection strings with credentials
  Protocol://user:password@host patterns where password is non-trivial
```

This list is the V1 baseline. New patterns can be added without
changing the contract structure. Removing a pattern requires a decision record.

---

## False Positive Handling

Secret scanning will produce false positives on:
- Test fixtures containing example keys (non-real)
- Documentation showing example token formats
- Escaped/base64-encoded sample values

Behavior on false positives:
- The content is still treated as secret (conservative).
- Operators may add path-level exceptions in the Attic configuration
  (e.g., `fixtures/example_keys/` → scan-exempt).
- Scan-exempt paths are explicitly listed in `DiscoveryPolicy`; they
  receive `security_state = SAFE` regardless of content.
- Scan-exempt paths CANNOT include paths with real infrastructure credentials.
- The exemption list is included in `discovery_policy_hash`.

---

## Partial Redaction

For files classified as `PARTIALLY_REDACTED`:

1. The scanner identifies all `RedactedSpan` ranges.
2. Content outside those spans is safe to index.
3. Redacted spans are replaced in-memory with `[REDACTED:<kind>]` before
   any downstream processing.
4. The redacted content (not the original) enters the analysis pipeline.
5. `FileOccurrence.security_state = PARTIALLY_REDACTED`.
6. The list of `(span, kind)` tuples is recorded in a secure log (not in
   the main DB) for auditing purposes only.

---

## Error Message Policy

Error messages from the secret scanner, logging subsystem, and all other
components MUST NOT quote or echo secret content, even when logging errors
about the secret itself.

Acceptable:
```
[ERROR] Secret detected in file src/config.py at byte range 145-189 (kind: API_KEY)
```

Not acceptable:
```
[ERROR] Invalid API key: sk_live_AbCdEfGhIjKlMnOpQr123456
```

---

## Invariants

1. Secret bytes NEVER enter `core_retrieval_units`, `fts_*`, `core_evidence`,
   or any other derived persistence layer.
2. `FileOccurrence.existence_state = SECRET_REDACTED` implies
   `retrieval_text` is NULL / empty for all associated retrieval units.
3. `security_state = FORBIDDEN` (path-level) takes precedence over all
   scanner results; the file is not read for scanning.
4. The redacted in-memory copy is discarded after analysis; only the indexed
   (redacted) artifacts are persisted.
5. Scan-exempt path rules do not override `FORBIDDEN` path-level exclusions.
6. No scan-exempt rule may use a pattern that could match
   `~/.ssh/`, `~/.gnupg/`, or any path in §Security Exclusions of the
   discovery contract.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Scanner crashes on a file | File treated as `SECRET_REDACTED`; diagnostic logged; indexing continues |
| Entropy calculation error | Fall back to pattern-only scan; log diagnostic |
| Partial redaction produces malformed content | Treat whole file as `SECRET_REDACTED`; do not index partial output |
| Scan-exempt config references a forbidden path | Configuration rejected at startup; binary exits with `CONFIG_INVALID` |

---

## Observability

Per-repository scan metrics (never including secret content):

```
files_scanned
files_safe
files_partially_redacted
files_secret_redacted
files_forbidden
redacted_spans_total
scan_errors
scan_duration_ms
```

---

## Examples

### .env file with real API key

```
File: .env
Content: DATABASE_URL=postgres://user:pass@localhost/db
         API_KEY=sk_live_AbCdEfGhIjKlMnOpQr123456

Result:
  SecretState: SECRET_REDACTED (API key pattern + whole-file policy for .env)
  FileOccurrence.existence_state: SECRET_REDACTED
  No retrieval_text created
  No FTS entry
```

### Source file with one hardcoded token

```
File: src/legacy/AuthClient.java
Content: ... (500 lines)
         private static final String TOKEN = "ghp_xXxXxXxXxXxXxXxXxXxXxXxXxXxXxX";
         ...

Result:
  SecretState: PARTIALLY_REDACTED
  RedactedSpan: covers the token value
  Indexed text: ... TOKEN = "[REDACTED:GITHUB_TOKEN]"; ...
  FTS entry: created from redacted content
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| SE-01 | File with no secrets | SecretState: SAFE; full content indexed |
| SE-02 | `.env` file with real API key | SECRET_REDACTED; no retrieval_text |
| SE-03 | Source file with one embedded GitHub token | PARTIALLY_REDACTED; token replaced; rest indexed |
| SE-04 | PEM private key file | SECRET_REDACTED; not indexed |
| SE-05 | Test fixture with example key (scan-exempt path) | SAFE; indexed normally |
| SE-06 | Scan-exempt config attempts to exempt `.ssh/` | Config rejected at startup |
| SE-07 | Scanner crash on one file | File: SECRET_REDACTED; diagnostic; other files continue |
| SE-08 | Error message for secret detection | Message contains range/kind; NEVER contains secret bytes |
| SE-09 | JWT in source comment | PARTIALLY_REDACTED; JWT span replaced |
| SE-10 | Binary file | Scan skipped; SAFE; binary handling per large_files contract |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| SE-Q1 | Should the secure audit log of redacted spans be kept in a separate file outside `index.db`? | No — separate file in `.mcp/state/`; never in main DB |
| SE-Q2 | Should Gitleaks or truffleHog pattern databases be integrated, or maintain our own? | No — own curated list for V1; external DB integration is Phase 7 |
| SE-Q3 | Should entropy threshold (4.5 bits/char) be configurable? | No — hardcoded for V1; configurable in Phase 7 |
