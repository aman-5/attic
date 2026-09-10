-- Semantic database migration: 0001_initial
--
-- Attic Phase 102 Clean Final Architecture baseline schema.
-- Contains the complete unified durable state for Qwen3-based semantic intelligence:
--   - Vector embeddings with generation isolation and repository-scoped composite indexes
--   - Durable crash-safe queue with priority and retry tracking
--   - Query demand tracking
--   - Semantic generations lifecycle
--   - Learned resource and throughput tuning per hardware/model/runtime

CREATE TABLE IF NOT EXISTS sem_schema_migrations (
    id          TEXT    PRIMARY KEY NOT NULL,
    applied_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000000)
);

CREATE TABLE IF NOT EXISTS sem_embeddings (
    retrieval_unit_id   TEXT    NOT NULL,
    repository_id       TEXT    NOT NULL,
    source_revision_id  TEXT    NOT NULL,
    index_generation_id TEXT    NOT NULL,
    selection_version   TEXT    NOT NULL,
    provider_id         TEXT    NOT NULL,
    model_id            TEXT    NOT NULL,
    content_hash        TEXT    NOT NULL,
    dim                 INTEGER NOT NULL,
    norm                REAL    NOT NULL,
    vector              BLOB    NOT NULL,
    created_at_ms       INTEGER NOT NULL,
    generation_id       INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (retrieval_unit_id, provider_id, model_id)
);

CREATE INDEX IF NOT EXISTS idx_sem_model
    ON sem_embeddings(provider_id, model_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen
    ON sem_embeddings(generation_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen_repo
    ON sem_embeddings(generation_id, repository_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_model_repo
    ON sem_embeddings(provider_id, model_id, repository_id);

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

CREATE TABLE IF NOT EXISTS sem_learned_tuning (
    tuning_key_hash           TEXT PRIMARY KEY NOT NULL,
    cpu_architecture          TEXT NOT NULL,
    os_name                   TEXT NOT NULL,
    model_id                  TEXT NOT NULL,
    model_revision            TEXT NOT NULL,
    dimension                 INTEGER NOT NULL,
    runtime_version           TEXT NOT NULL,
    recommended_lanes         INTEGER NOT NULL,
    recommended_batch_size    INTEGER NOT NULL,
    recommended_cpu_threads   INTEGER NOT NULL,
    observed_chunks_per_sec   REAL NOT NULL,
    updated_at_ms             INTEGER NOT NULL
);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0001_initial', strftime('%s', 'now') * 1000000);
