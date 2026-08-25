-- ============================================================
-- Migration 0003: Phase 2 — Incremental Correctness and Freshness
-- (ADR-009)
--
-- Idempotent DDL only.  No existing column or table is altered.
-- ============================================================

-- Cross-revision file-identity continuation links (identity contract:
-- "Cross-occurrence continuity is a separate heuristic step; it does not
-- mutate identity records themselves").  Confidence is always explicit.
CREATE TABLE IF NOT EXISTS core_identity_links (
    id               TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id    TEXT    NOT NULL REFERENCES core_repositories(id),
    from_identity_id TEXT    NOT NULL REFERENCES core_file_identities(id),
    to_identity_id   TEXT    NOT NULL REFERENCES core_file_identities(id),
    prior_path       TEXT    NOT NULL,
    new_path         TEXT    NOT NULL,
    -- confidence enum (identity contract): EXACT | HEURISTIC | NONE
    confidence       TEXT    NOT NULL,
    -- basis enum: GIT_RENAME | CONTENT_MATCH | NONE
    basis            TEXT    NOT NULL,
    created_at       INTEGER NOT NULL                -- microseconds since Unix epoch (UTC)
);

CREATE INDEX IF NOT EXISTS idx_identity_links_repo
    ON core_identity_links(repository_id);
CREATE INDEX IF NOT EXISTS idx_identity_links_from
    ON core_identity_links(from_identity_id);
CREATE INDEX IF NOT EXISTS idx_identity_links_to
    ON core_identity_links(to_identity_id);

-- Enqueue dedup support for idempotent task creation (ADR-009 §2).
CREATE INDEX IF NOT EXISTS idx_tasks_pending_dedup
    ON ops_tasks(task_type, repository_id)
    WHERE state = 'PENDING';

CREATE INDEX IF NOT EXISTS idx_tasks_type_state
    ON ops_tasks(task_type, state);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
    VALUES ('0003_phase2', strftime('%s', 'now') * 1000000);

INSERT OR IGNORE INTO ops_migration_log (id, status, started_at, completed_at)
    VALUES ('0003_phase2', 'COMPLETED',
            strftime('%s', 'now') * 1000000,
            strftime('%s', 'now') * 1000000);
