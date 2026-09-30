-- Semantic database schema — the single baseline.
--
--   sem_embeddings            per-generation vector projection behind the HNSW index
--   sem_query_demand          query demand used by the selection policy
--   sem_generations           semantic generation lifecycle
--   sem_embeddings_v2         ONE canonical vector per (vector space, content hash);
--                             vector_space_id / content_generation_id are derived
--                             from the provider fingerprint
--   sem_embedding_occurrences every place a canonical body occurs
--   sem_queue_v2              leased/fenced embedding work queue: a claim sets an
--                             owner, expiry and fencing token; a commit carrying an
--                             older token is rejected
--
-- semantic.db is disposable: a database created by any other schema is wiped
-- and rebuilt automatically (see SemanticStore::migrate).

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
    PRIMARY KEY (retrieval_unit_id, provider_id, model_id, generation_id)
);

CREATE INDEX IF NOT EXISTS idx_sem_model
    ON sem_embeddings(provider_id, model_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen
    ON sem_embeddings(generation_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen_repo
    ON sem_embeddings(generation_id, repository_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_model_repo
    ON sem_embeddings(provider_id, model_id, repository_id);

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

CREATE TABLE IF NOT EXISTS sem_embeddings_v2 (
    vector_space_id TEXT    NOT NULL,
    canonical_hash  TEXT    NOT NULL,
    dim             INTEGER NOT NULL,
    norm            REAL    NOT NULL,
    vector          BLOB    NOT NULL,
    created_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (vector_space_id, canonical_hash)
);

CREATE TABLE IF NOT EXISTS sem_embedding_occurrences (
    occurrence_id         TEXT PRIMARY KEY,
    retrieval_unit_id     TEXT NOT NULL,
    vector_space_id       TEXT NOT NULL,
    canonical_hash        TEXT NOT NULL,
    repository_id         TEXT NOT NULL,
    source_revision_id    TEXT NOT NULL,
    index_generation_id   TEXT NOT NULL,
    content_generation_id TEXT NOT NULL,
    -- Per-occurrence provenance: {"json_pointer": "...", "environment": "..."}.
    metadata_json         TEXT NOT NULL DEFAULT '{}',
    created_at_ms         INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_sem_occ_unit
    ON sem_embedding_occurrences(retrieval_unit_id);
CREATE INDEX IF NOT EXISTS idx_sem_occ_canonical
    ON sem_embedding_occurrences(vector_space_id, canonical_hash);
CREATE INDEX IF NOT EXISTS idx_sem_occ_generation
    ON sem_embedding_occurrences(index_generation_id);

CREATE TABLE IF NOT EXISTS sem_queue_v2 (
    occurrence_id       TEXT PRIMARY KEY REFERENCES sem_embedding_occurrences(occurrence_id),
    priority            REAL    NOT NULL DEFAULT 0.5,
    state               TEXT    NOT NULL DEFAULT 'PENDING',  -- PENDING | INFLIGHT | DONE | FAILED
    attempts            INTEGER NOT NULL DEFAULT 0,
    enqueued_at_ms      INTEGER NOT NULL,
    -- Lease supervision: a claim sets owner+expiry and bumps fencing_token.
    -- Heartbeats extend the lease. Reclaim after expiry bumps the token
    -- again, so any commit carrying an older token is rejected as stale.
    lease_owner         TEXT,
    lease_expires_at_ms INTEGER,
    heartbeat_at_ms     INTEGER,
    fencing_token       INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_ms  INTEGER,
    last_error          TEXT
);

CREATE INDEX IF NOT EXISTS idx_sem_queue_v2_state
    ON sem_queue_v2(state, priority DESC, enqueued_at_ms);
-- Reclaim scan: expired INFLIGHT leases surface first.
CREATE INDEX IF NOT EXISTS idx_sem_queue_v2_lease
    ON sem_queue_v2(state, lease_expires_at_ms);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0001_initial', strftime('%s', 'now') * 1000000);
