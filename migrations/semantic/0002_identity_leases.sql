-- 0002_identity_leases (r02): durable vector-space identity, canonical
-- embeddings with occurrence links, and a leased/fenced work queue.
--
-- Design contract:
-- * sem_vector_spaces       — one row per exact vector identity (model,
--                             revision, quantization, dims, tokenizer,
--                             pooling, normalization, instruction). Changing
--                             ANY component yields a new id and new vectors;
--                             old spaces stay queryable for rollback.
-- * sem_content_generations — one row per exact content pipeline identity
--                             (chunking, canonicalization, analyzer registry,
--                             selection). Content changes never invalidate
--                             vectors; only occurrence mapping rebuilds.
-- * sem_embeddings_v2       — ONE vector per (vector_space, canonical_hash).
--                             This is the canonical dedup unit.
-- * sem_embedding_occurrences — every place that canonical content occurs:
--                             repo, path unit, JSON pointer, environment.
-- * sem_queue_v2            — worker-supervision queue with lease owner,
--                             expiry, heartbeat, and fencing token. A stale
--                             worker (superseded lease) must be unable to
--                             commit: every mutation checks fencing_token.
--
-- sem_queue / sem_embeddings stay in place for rollback until r06 cuts the
-- enrichment pipeline over to v2.

CREATE TABLE IF NOT EXISTS sem_vector_spaces (
    vector_space_id           TEXT PRIMARY KEY,
    provider_id               TEXT NOT NULL,
    model_id                  TEXT NOT NULL,
    model_revision            TEXT NOT NULL,
    quantization              TEXT NOT NULL DEFAULT 'unknown',
    dim                       INTEGER NOT NULL,
    pooling_version           TEXT NOT NULL,
    normalization_version     TEXT NOT NULL,
    tokenizer_version         TEXT NOT NULL,
    query_instruction_version TEXT NOT NULL,
    execution_backend         TEXT NOT NULL DEFAULT 'unknown',
    created_at_ms             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sem_content_generations (
    content_generation_id     TEXT PRIMARY KEY,
    chunking_version          TEXT NOT NULL,
    canonicalization_version  TEXT NOT NULL,
    analyzer_registry_version TEXT NOT NULL,
    selection_version         TEXT NOT NULL,
    created_at_ms             INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sem_embeddings_v2 (
    vector_space_id TEXT    NOT NULL REFERENCES sem_vector_spaces(vector_space_id),
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

INSERT OR IGNORE INTO sem_schema_migrations (id) VALUES ('0002_identity_leases');
