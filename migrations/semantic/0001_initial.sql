-- Semantic database migration: 0001_initial
--
-- The semantic subsystem intentionally uses a separate, disposable SQLite
-- database (`semantic.db`). Keep its durable schema migration-owned without
-- mixing provider-specific semantic tables into the canonical `attic.db`.
--
-- This migration is safe for existing semantic databases because all schema
-- creation is idempotent. Existing databases that were previously created by
-- `SemanticStore::migrate` are adopted by recording this migration after the
-- compatible tables/indexes have been verified/created.

CREATE TABLE IF NOT EXISTS sem_schema_migrations (
    id          TEXT    PRIMARY KEY NOT NULL,
    applied_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000000)
);

CREATE TABLE IF NOT EXISTS sem_embeddings (
    retrieval_unit_id  TEXT    NOT NULL,
    repository_id      TEXT    NOT NULL,
    source_revision_id TEXT    NOT NULL,
    index_generation_id TEXT   NOT NULL,
    selection_version  TEXT    NOT NULL,
    provider_id        TEXT    NOT NULL,
    model_id           TEXT    NOT NULL,
    content_hash       TEXT    NOT NULL,
    dim                INTEGER NOT NULL,
    norm               REAL    NOT NULL,
    vector             BLOB    NOT NULL,
    created_at_ms      INTEGER NOT NULL,
    PRIMARY KEY (retrieval_unit_id, provider_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_sem_model
    ON sem_embeddings(provider_id, model_id);

CREATE TABLE IF NOT EXISTS sem_queue (
    retrieval_unit_id TEXT PRIMARY KEY,
    priority          REAL    NOT NULL DEFAULT 0.5,
    state             TEXT    NOT NULL DEFAULT 'PENDING',
    attempts          INTEGER NOT NULL DEFAULT 0,
    enqueued_at_ms    INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sem_queue_state
    ON sem_queue(state, priority DESC, enqueued_at_ms);

CREATE TABLE IF NOT EXISTS sem_query_demand (
    path       TEXT PRIMARY KEY,
    hits       INTEGER NOT NULL DEFAULT 0,
    last_at_ms INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0001_initial', strftime('%s', 'now') * 1000000);
