-- Migration: 0002_phase1d
-- Description: Phase 1D additions — retrieval unit columns for FTS pipeline and MCP tools.
-- Applies to: workspace/.mcp/index.db
-- Schema version: 1.1.0
--
-- Adds indexing metadata columns to core_retrieval_units so the FTS search
-- results can carry full provenance without additional joins.
-- Adds ret_retrieval_units as the canonical Phase 1D working table (replaces
-- the stub used in Phase 1A tests).

-- ============================================================
-- SECTION 1: Extend core_retrieval_units with Phase 1D columns
-- ============================================================

-- NOTE: repository_id already exists in core_retrieval_units (added in 0001_initial).
-- It is NOT re-added here.

-- analyzer_id: identifier of the analyzer that produced this unit.
ALTER TABLE core_retrieval_units ADD COLUMN analyzer_id TEXT;

-- analyzer_version: version of the analyzer that produced this unit.
ALTER TABLE core_retrieval_units ADD COLUMN analyzer_version TEXT;

-- start_line: 0-based start line of the retrieval unit in the source file.
ALTER TABLE core_retrieval_units ADD COLUMN start_line INTEGER;

-- end_line: 0-based end line (inclusive) of the retrieval unit in the source file.
ALTER TABLE core_retrieval_units ADD COLUMN end_line INTEGER;

-- is_redacted: 1 if this unit's retrieval_text was redacted by Phase 1B secrets.
ALTER TABLE core_retrieval_units ADD COLUMN is_redacted INTEGER NOT NULL DEFAULT 0;

-- ============================================================
-- SECTION 2: Index on repository_id for scoped search
-- ============================================================

CREATE INDEX IF NOT EXISTS idx_retrieval_units_repository
    ON core_retrieval_units(repository_id);

-- ============================================================
-- SECTION 3: Record this migration
-- ============================================================

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at)
    VALUES ('0002_phase1d', strftime('%s', 'now') * 1000000);

INSERT OR IGNORE INTO ops_migration_log (id, status, started_at, completed_at)
    VALUES ('0002_phase1d', 'COMPLETED',
            strftime('%s', 'now') * 1000000,
            strftime('%s', 'now') * 1000000);
