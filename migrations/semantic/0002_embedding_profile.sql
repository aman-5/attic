-- Semantic database migration: 0002_embedding_profile
--
-- Persists the ACTIVE embedding vector-space identity exactly once, at first
-- real embedding work (never merely on startup/status). `singleton_guard`'s
-- `CHECK (singleton_guard = 1)` primary key is the standard SQLite idiom for
-- an "exactly one row" table, making the first-claim race resolvable with a
-- plain `INSERT ... ON CONFLICT(singleton_guard) DO NOTHING`.

CREATE TABLE IF NOT EXISTS sem_embedding_profile (
    singleton_guard INTEGER PRIMARY KEY CHECK (singleton_guard = 1),
    profile_id      TEXT NOT NULL,   -- BLAKE3 hex hash of the canonical EmbeddingSpaceDescriptor
    config_json     TEXT NOT NULL,   -- canonical serialized EmbeddingSpaceDescriptor
    claimed_at_ms   INTEGER NOT NULL
);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0002_embedding_profile', strftime('%s', 'now') * 1000000);
