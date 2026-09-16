-- 0002_retrieval_occurrence (r02): canonical content / occurrence split.
--
-- A retrieval unit's SEARCHABLE text (retrieval_text) may legitimately carry
-- per-occurrence decoration (JSON pointer headers, environment labels). The
-- semantic layer must hash and embed the CANONICAL body instead, so identical
-- logical content across files/environments/pointers reuses one embedding
-- while every occurrence remains individually addressable.
--
-- canonical_text      — exact text handed to the embedding provider; NULL for
--                       pre-0002 rows (legacy units are canonical-by-identity:
--                       their retrieval_text WAS the embedded text).
-- canonical_hash      — BLAKE3 hex of canonical_text (or retrieval_text for
--                       legacy rows at write time). NOT NULL for new rows.
-- occurrence_metadata — JSON object with per-occurrence provenance, e.g.
--                       {"json_pointer": "/services/0", "environment": "PROD"}.
--                       Never part of the hash.
-- coverage_state      — COMPLETE | PARTIAL | TRUNCATED. r01 made analysis
--                       fail-closed, so only COMPLETE rows are ever published
--                       today; the column exists so future spill/streaming
--                       work (r03) can represent partial truth explicitly.

ALTER TABLE core_retrieval_units ADD COLUMN canonical_text TEXT;
ALTER TABLE core_retrieval_units ADD COLUMN canonical_hash TEXT;
ALTER TABLE core_retrieval_units ADD COLUMN occurrence_metadata TEXT;
ALTER TABLE core_retrieval_units ADD COLUMN coverage_state TEXT NOT NULL DEFAULT 'COMPLETE';

-- Canonical-content lookup: enrichment finds identical bodies across the
-- whole workspace without scanning retrieval_text.
CREATE INDEX idx_retrieval_units_canonical_hash
    ON core_retrieval_units(canonical_hash);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at) VALUES ('0002_retrieval_occurrence', strftime('%s', 'now') * 1000000);
