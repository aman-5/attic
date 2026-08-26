-- Migration: 0004_phase6
-- Description: Cross-repository intelligence (Phase 6).
--   - core_workspace_catalog        : DERIVED per-repository workspace
--     catalog row (provided identities as JSON, manifest hash, freshness).
--     Rebuildable at any time; never an authoritative source of truth.
--   - core_dependency_declarations  : parsed build/package dependency
--     declarations per repository (derived from untrusted manifest bytes).
--
-- Resolved cross-repository EDGES live in core_relationships
-- (rel_type='DEPENDS_ON', source_repository_id != target_repository_id) so
-- Phase 4 graph expansion, freshness handling and invalidation apply
-- unchanged. Unresolved/ambiguous targets are NEVER persisted as edges.
--
-- All DDL is idempotent. Secret bytes must never be stored in any column;
-- parsers never copy raw manifest content into outputs.

CREATE TABLE IF NOT EXISTS core_workspace_catalog (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL UNIQUE REFERENCES core_repositories(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- JSON array of {"ecosystem": "...", "name": "..."} provided identities.
    provides_json       TEXT    NOT NULL DEFAULT '[]',
    -- BLAKE3 over sorted (path, content_hash) of declaration files; change
    -- detector for cheap incremental catalog refresh decisions.
    manifest_hash       TEXT    NOT NULL DEFAULT '',
    entry_count         INTEGER NOT NULL DEFAULT 0,
    -- CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS core_dependency_declarations (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    -- Declaring manifest occurrence when known (NULL for synthetic rows).
    file_occurrence_id  TEXT REFERENCES core_file_occurrences(id),
    path                TEXT    NOT NULL,
    -- MAVEN | GRADLE | GO | NPM | PYTHON | SUBMODULE | CONFIG | GENERATED_API
    ecosystem           TEXT    NOT NULL,
    name                TEXT    NOT NULL,
    version_req         TEXT,
    -- external | local_path | workspace_member
    declaration_kind    TEXT    NOT NULL,
    local_hint          TEXT,
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_xrepo_decls_repo
    ON core_dependency_declarations(repository_id);
CREATE INDEX IF NOT EXISTS idx_xrepo_decls_name
    ON core_dependency_declarations(ecosystem, name);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
VALUES ('0004_phase6', strftime('%s', 'now') * 1000000);
