-- Migration: 0007_analysis_cache_versioning
-- Description: version-stamp index_analysis_cache entries (code-review fix).
--
-- A cache entry is keyed by content_hash alone, but content hash does not
-- capture everything that determines a file's analysis result: a retry
-- that spans a secret-detector or analyzer-registry upgrade must never
-- replay a verdict computed under the old ruleset for unchanged content
-- (e.g. the upgraded detector now recognizes a secret it previously
-- missed). Stamping each entry with the versions active when it was
-- computed lets the read path treat a version mismatch as a cache miss.
--
-- All DDL is idempotent.

ALTER TABLE index_analysis_cache
    ADD COLUMN secret_pattern_version INTEGER NOT NULL DEFAULT 1;

ALTER TABLE index_analysis_cache
    ADD COLUMN analyzer_registry_version TEXT NOT NULL DEFAULT '';

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
VALUES ('0007_analysis_cache_versioning', strftime('%s', 'now') * 1000000);
