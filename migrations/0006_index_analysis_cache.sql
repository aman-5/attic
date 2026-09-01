-- Migration: 0006_index_analysis_cache
-- Description: retry-isolation cache for the full-index pipeline (PR-7).
--
-- Full indexing analyzes every discovered file in memory, then publishes
-- everything as ONE atomic writer-queue transaction at the very end
-- (see submit_index_publication) — a single generation is never partially
-- current. If one file fails transiently after 999 others succeeded, the
-- whole run aborts before publication and nothing is written, so a naive
-- retry would re-run analysis (parsing, secret scanning, structural
-- extraction) for all 1000 files again.
--
-- This table caches each successfully-analyzed file's result, keyed by
-- content hash so a changed file is never served a stale result. On a
-- transient-failure abort, whatever succeeded so far is persisted here in
-- one writer-queue submission; the next attempt (retry, or after a process
-- restart) bulk-loads this cache and skips re-analysis for any path whose
-- current content hash still matches. This is purely a cache in front of
-- analysis — it does not change the atomic publish path, the generation
-- completeness gate, or the CURRENT-generation invariant at all: a cache
-- miss degrades exactly to today's behavior (recompute).
--
-- Cleared for a repository once a full-index run publishes successfully —
-- the cache is no longer needed and would otherwise grow unbounded.
--
-- All DDL is idempotent.

CREATE TABLE IF NOT EXISTS index_analysis_cache (
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    repo_relative   TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,
    security_state  TEXT    NOT NULL,
    is_partial_scan INTEGER NOT NULL,
    -- JSON-serialized Vec<PendingUnit>-equivalent retrieval units.
    units_json      TEXT    NOT NULL,
    -- JSON-serialized structural capture (Option<CapturedFile>); NULL when
    -- the file produced no structural intelligence.
    captured_json   TEXT,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (repository_id, repo_relative)
);

CREATE INDEX IF NOT EXISTS idx_index_analysis_cache_repo
    ON index_analysis_cache(repository_id);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
VALUES ('0006_index_analysis_cache', strftime('%s', 'now') * 1000000);
