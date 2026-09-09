-- Semantic database migration: 0003_semantic_generations
--
-- Adds support for discrete, isolated semantic generations (Master Plan V2 §52–§55).
-- Allows building new embedding vector spaces (e.g. Qwen3) in the background while
-- actively serving from the existing generation (e.g. BGE), activating atomically,
-- and rolling back safely without vector space mixing.

CREATE TABLE IF NOT EXISTS sem_generations (
    generation_id    INTEGER PRIMARY KEY AUTOINCREMENT,
    fingerprint_json TEXT NOT NULL,
    fingerprint_hash TEXT NOT NULL,
    status           TEXT NOT NULL, -- 'BUILDING', 'ACTIVE', 'SUPERSEDED', 'ROLLEDBACK'
    unit_count       INTEGER NOT NULL DEFAULT 0,
    created_at_ms    INTEGER NOT NULL,
    activated_at_ms  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_sem_gen_status
    ON sem_generations(status);

ALTER TABLE sem_embeddings ADD COLUMN generation_id INTEGER NOT NULL DEFAULT 1;

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen
    ON sem_embeddings(generation_id);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0003_semantic_generations', strftime('%s', 'now') * 1000000);
