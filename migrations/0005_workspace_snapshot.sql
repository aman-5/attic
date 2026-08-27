-- Migration: 0005_workspace_snapshot
-- Description: WorkspaceSnapshot provenance for cross-repository conclusions (Phase 6).
--
-- A cross-repo conclusion (edge set from sync_workspace) depends on multiple
-- repository revisions concurrently. This migration captures the revision set
-- that backed each workspace resolution run so downstream consumers can
-- identify exactly which (repository_id, source_revision_id) pairs formed the
-- basis for any cross-repo claim.
--
-- Design invariants:
--   - One snapshot row per sync_workspace run.
--   - One revision entry per repository that participated (had a SourceRevision
--     and contributed to the resolver input).
--   - Snapshots are immutable after creation; never updated in-place.
--   - Rows accumulate until vacuumed; old snapshots do NOT invalidate edges.
--   - The current (most recent) snapshot for a workspace is the one with the
--     highest created_at. Edges produced during that run reference its id via
--     provenance_json; no FK to avoid cascade issues with edge churn.
--
-- All DDL is idempotent.

CREATE TABLE IF NOT EXISTS core_workspace_snapshots (
    id              TEXT    NOT NULL PRIMARY KEY,
    created_at      INTEGER NOT NULL,
    repo_count      INTEGER NOT NULL DEFAULT 0,
    edges_emitted   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS core_workspace_snapshot_revisions (
    id                  TEXT    NOT NULL PRIMARY KEY,
    snapshot_id         TEXT    NOT NULL,
    repository_id       TEXT    NOT NULL,
    source_revision_id  TEXT    NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_ws_snapshot_revisions_snapshot
    ON core_workspace_snapshot_revisions(snapshot_id);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
VALUES ('0005_workspace_snapshot', strftime('%s', 'now') * 1000000);
