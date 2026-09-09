-- Semantic database migration: 0005_vector_index_scale
--
-- Optimizes vector search scalability under large multi-repository indexes (Master Plan V2 §60, CP20).
-- Adds a composite index on (generation_id, repository_id) to skip non-matching repositories
-- at B-tree index speeds during scoped kNN queries.

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_gen_repo
    ON sem_embeddings(generation_id, repository_id);

CREATE INDEX IF NOT EXISTS idx_sem_embeddings_model_repo
    ON sem_embeddings(provider_id, model_id, repository_id);

INSERT OR IGNORE INTO sem_schema_migrations (id, applied_at)
VALUES ('0005_vector_index_scale', strftime('%s', 'now') * 1000000);
