-- Semantic database migration: 0004_learned_tuning
--
-- Persists learned resource and throughput tuning per machine, model, and runtime (Master Plan V2 §25).
-- Allows fast cold-start using previously verified optimal lane count, batch size, and CPU grant,
-- while invalidating automatically when hardware, OS, model fingerprint, dimension, or runtime version changes.

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
VALUES ('0004_learned_tuning', strftime('%s', 'now') * 1000000);
