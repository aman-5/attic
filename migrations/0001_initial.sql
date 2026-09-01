-- Attic canonical database baseline schema for pre-release QA.
-- Fresh databases only; pre-QA development migration history was intentionally squashed.
PRAGMA foreign_keys = ON;

CREATE TABLE core_dependency_declarations (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    -- Declaring manifest occurrence when known (NULL for synthetic rows).
    file_occurrence_id  TEXT REFERENCES core_file_occurrences(id),
    path                TEXT    NOT NULL,
    -- MAVEN | GRADLE | GO | NPM | PYTHON | SUBMODULE | CONFIG | GENERATED_API
    ecosystem           TEXT    NOT NULL,
    name                TEXT    NOT NULL,
    version_req         TEXT,
    -- external | local_path | workspace_member
    declaration_kind    TEXT    NOT NULL,
    local_hint          TEXT,
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE core_evidence (
    id                      TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id           TEXT    NOT NULL REFERENCES core_repositories(id),
    -- source_type enum: SOURCE_CODE | TEST | CONFIGURATION | DOCUMENTATION | KNOWLEDGE | RELATIONSHIP | GENERATED_SOURCE
    source_type             TEXT    NOT NULL,
    source_id               TEXT    NOT NULL,   -- FK to file_occurrence_id or knowledge_item_id
    path                    TEXT    NOT NULL,   -- workspace-relative normalized path
    source_revision_id      TEXT    NOT NULL REFERENCES core_source_revisions(id),
    index_generation_id     TEXT    NOT NULL REFERENCES core_index_generations(id),
    source_span             TEXT,               -- NULL for whole-file evidence
    content_hash            TEXT    NOT NULL,   -- BLAKE3 hex of evidence content
    -- freshness_state enum: CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state         TEXT    NOT NULL DEFAULT 'CURRENT',
    -- authority enum: SOURCE_CODE | TEST | KNOWLEDGE | CONFIGURATION | RELATIONSHIP
    authority               TEXT    NOT NULL,
    confidence              REAL    NOT NULL DEFAULT 1.0,
    relationship_confidence REAL,               -- NULL if not derived from a relationship
    -- verification_state enum: UNVERIFIED | VERIFIED | STALE | CONTRADICTED
    verification_state      TEXT    NOT NULL DEFAULT 'UNVERIFIED',
    ranking_signals_json    TEXT    -- JSON object of RankingSignals; no secret content
);

CREATE TABLE core_file_identities (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    stable_id_basis TEXT    NOT NULL   -- basis for stable identity (Git blob SHA or path hash)
);

CREATE TABLE core_file_occurrences (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    file_identity_id    TEXT    NOT NULL REFERENCES core_file_identities(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    index_generation_id TEXT    REFERENCES core_index_generations(id),  -- NULL if not yet indexed
    path                TEXT    NOT NULL,   -- workspace-relative normalized path (forward slashes)
    content_hash        TEXT    NOT NULL,   -- BLAKE3 hex of raw file bytes
    size_bytes          INTEGER NOT NULL,
    language            TEXT,               -- NULL for binary/unknown
    -- file_type enum: SOURCE | CONFIG | DOCUMENT | INFRA | GENERATED | BINARY | UNKNOWN
    file_type           TEXT    NOT NULL,
    -- discovery_class enum: IGNORED | LOW_PRIORITY | NORMAL | HIGH_PRIORITY
    discovery_class     TEXT    NOT NULL,
    -- security_state enum: SAFE | FORBIDDEN | SECRET_REDACTED | PARTIALLY_REDACTED
    security_state      TEXT    NOT NULL,
    -- existence_state enum: ACTIVE | DELETED | EXCLUDED | INACCESSIBLE | TOO_LARGE | BINARY | SECRET_REDACTED | PARSER_FAILED
    existence_state     TEXT    NOT NULL,
    -- freshness_state enum: CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',
    -- secret_scan_state enum: PENDING | IN_PROGRESS | CLEAN | HAS_SECRETS
    secret_scan_state   TEXT    NOT NULL DEFAULT 'PENDING',
    -- OQ-007 (ADR-002): which secret detector pattern version last scanned this file to CLEAN/HAS_SECRETS
    secret_pattern_version  INTEGER NOT NULL DEFAULT 1,
    last_indexed_at     INTEGER   -- microseconds since Unix epoch; NULL if never indexed
);

CREATE TABLE core_identity_links (
    id               TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id    TEXT    NOT NULL REFERENCES core_repositories(id),
    from_identity_id TEXT    NOT NULL REFERENCES core_file_identities(id),
    to_identity_id   TEXT    NOT NULL REFERENCES core_file_identities(id),
    prior_path       TEXT    NOT NULL,
    new_path         TEXT    NOT NULL,
    -- confidence enum (identity contract): EXACT | HEURISTIC | NONE
    confidence       TEXT    NOT NULL,
    -- basis enum: GIT_RENAME | CONTENT_MATCH | NONE
    basis            TEXT    NOT NULL,
    created_at       INTEGER NOT NULL                -- microseconds since Unix epoch (UTC)
);

CREATE TABLE core_index_generations (
    id                        TEXT    NOT NULL PRIMARY KEY,   -- UUID
    source_revision_id        TEXT    NOT NULL REFERENCES core_source_revisions(id),
    schema_version            TEXT    NOT NULL,   -- semver string, e.g., "1.0.0"
    analyzer_registry_version TEXT    NOT NULL,   -- semver string
    analyzer_versions_json    TEXT    NOT NULL,   -- JSON object: { "analyzer_id": "version", ... }
    segmentation_version      TEXT    NOT NULL,   -- semver string
    indexer_version           TEXT    NOT NULL,   -- semver string
    discovery_policy_hash     TEXT    NOT NULL,   -- SHA-256 hex
    ranking_version           TEXT    NOT NULL,   -- semver string
    embedding_model_version   TEXT,               -- NULL if no semantic index in this generation
    configuration_hash        TEXT    NOT NULL,   -- SHA-256 hex of full startup configuration
    -- OQ-007 (ADR-002): secret detector pattern version active when this generation was produced
    secret_detector_version   INTEGER NOT NULL DEFAULT 1,
    -- OQ-016 (ADR-004): consolidated subsystem_key → version map for compatibility checks
    subsystem_versions_json   TEXT    NOT NULL,   -- JSON: { "schema": "1.0.0", "analyzer.<id>": "...", ... }
    created_at                INTEGER NOT NULL    -- microseconds since Unix epoch (UTC)
);

CREATE TABLE core_invalidation_records (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID
    -- artifact_type enum: FILE_OCCURRENCE | STRUCTURAL_NODE | SYMBOL_OCCURRENCE |
    --                      RETRIEVAL_UNIT | SEMANTIC_REPR | RELATIONSHIP | EVIDENCE
    artifact_type   TEXT    NOT NULL,
    artifact_id     TEXT    NOT NULL,
    -- reason enum: SOURCE_CHANGED | ANALYZER_UPGRADED | SCHEMA_MIGRATED | POLICY_CHANGED | EXPLICIT
    reason          TEXT    NOT NULL,
    invalidated_at  INTEGER NOT NULL,   -- microseconds since Unix epoch (UTC)
    recomputed_at   INTEGER             -- NULL until the artifact has been recomputed
);

CREATE TABLE core_knowledge_items (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    source              TEXT    NOT NULL,   -- e.g., "knowledge/architecture.md"
    -- authority enum: SOURCE_CODE | TEST | KNOWLEDGE | CONFIGURATION | RELATIONSHIP
    authority           TEXT    NOT NULL,
    last_verified_at    INTEGER,            -- microseconds since Unix epoch; NULL if never verified
    applicable_versions TEXT,               -- semver range string; NULL if version-agnostic
    supersedes_id       TEXT    REFERENCES core_knowledge_items(id),  -- NULL if not a supersession
    confidence          REAL    NOT NULL DEFAULT 1.0,  -- [0.0, 1.0]
    content_hash        TEXT    NOT NULL,   -- BLAKE3 hex
    -- freshness_state enum: CURRENT | STALE | INVALID
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE TABLE core_relationships (
    id                    TEXT    NOT NULL PRIMARY KEY,   -- UUID
    source_repository_id  TEXT    NOT NULL REFERENCES core_repositories(id),
    source_entity_id      TEXT    NOT NULL,   -- FK to file_occurrence_id or symbol_occurrence_id
    -- source_entity_type enum: FILE_OCCURRENCE | SYMBOL_OCCURRENCE
    source_entity_type    TEXT    NOT NULL,
    target_repository_id  TEXT    NOT NULL REFERENCES core_repositories(id),
    target_entity_id      TEXT    NOT NULL,
    -- target_entity_type enum: FILE_OCCURRENCE | SYMBOL_OCCURRENCE
    target_entity_type    TEXT    NOT NULL,
    -- rel_type enum: IMPORT | CALL | EXTENDS | IMPLEMENTS | DEPENDS_ON | OVERRIDES | REFERENCES | TESTS | CONFIGURES | OTHER
    rel_type              TEXT    NOT NULL,
    -- dependency_basis enum: MAVEN | GRADLE | GO_MODULE | NPM | PYTHON_PACKAGE | IMPORT | HEURISTIC | OTHER
    dependency_basis      TEXT    NOT NULL,
    -- resolution enum: SYNTACTIC | PACKAGE_RESOLVED | SYMBOL_RESOLVED | BUILD_RESOLVED | FRAMEWORK_RESOLVED | INFERRED
    resolution            TEXT    NOT NULL,
    confidence            REAL    NOT NULL DEFAULT 1.0,  -- [0.0, 1.0]
    provenance_json       TEXT,               -- structured provenance metadata; no secret content
    source_revision_id    TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- freshness_state enum: CURRENT | STALE | INVALID
    freshness_state       TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE TABLE core_repositories (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID (lowercase hyphenated)
    root_path       TEXT    NOT NULL UNIQUE,        -- canonical absolute path (UTF-8, normalized)
    display_name    TEXT    NOT NULL,
    is_git          INTEGER NOT NULL DEFAULT 1,     -- BOOLEAN: 1=true, 0=false
    case_sensitive  INTEGER NOT NULL DEFAULT 1,     -- BOOLEAN: filesystem case sensitivity
    created_at      INTEGER NOT NULL,               -- microseconds since Unix epoch (UTC)
    updated_at      INTEGER NOT NULL                -- microseconds since Unix epoch (UTC)
);

CREATE TABLE core_retrieval_unit_nodes (
    retrieval_unit_id   TEXT    NOT NULL REFERENCES core_retrieval_units(id),
    structural_node_id  TEXT    NOT NULL REFERENCES core_structural_nodes(id),
    ordinal             INTEGER NOT NULL,   -- ordering of nodes within this retrieval unit
    PRIMARY KEY (retrieval_unit_id, structural_node_id)
);

CREATE TABLE core_retrieval_units (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    index_generation_id TEXT    NOT NULL REFERENCES core_index_generations(id),
    -- retrieval_text: searchable text; MUST NOT contain secret bytes (contract: secrets.md)
    retrieval_text      TEXT    NOT NULL,
    -- lexical_state enum: CURRENT | STALE | INVALID
    lexical_state       TEXT    NOT NULL DEFAULT 'CURRENT',
    -- semantic_state enum: NONE | PENDING | CURRENT | STALE
    semantic_state      TEXT    NOT NULL DEFAULT 'NONE',
    -- freshness_state enum: CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
, analyzer_id TEXT, analyzer_version TEXT, start_line INTEGER, end_line INTEGER, is_redacted INTEGER NOT NULL DEFAULT 0);

CREATE TABLE IF NOT EXISTS core_schema_migrations (
    id          TEXT    NOT NULL PRIMARY KEY,  -- e.g., "0001_initial"
    applied_at  INTEGER NOT NULL               -- Unix timestamp microseconds (UTC)
);

CREATE TABLE core_source_revisions (
    id                          TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id               TEXT    NOT NULL REFERENCES core_repositories(id),
    commit_sha                  TEXT,               -- NULL if non-Git repository
    branch                      TEXT,               -- NULL if non-Git or detached HEAD
    working_tree_manifest_hash  TEXT    NOT NULL,   -- BLAKE3 hex of sorted eligible-file manifest
    discovery_policy_hash       TEXT    NOT NULL,   -- SHA-256 hex of serialized DiscoveryPolicy
    unstable_capture            INTEGER NOT NULL DEFAULT 0,  -- BOOLEAN: 1 if dirty/untracked files present
    captured_at                 INTEGER NOT NULL    -- microseconds since Unix epoch (UTC)
);

CREATE TABLE core_structural_nodes (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    parent_id           TEXT    REFERENCES core_structural_nodes(id),   -- NULL for root node
    node_type           TEXT    NOT NULL,   -- analyzer-defined type string
    structural_identity TEXT    NOT NULL,   -- stable identity basis for rename detection
    source_span         TEXT    NOT NULL,   -- "start_line:start_col-end_line:end_col"
    content_hash        TEXT    NOT NULL,   -- BLAKE3 hex of node content bytes
    analyzer_id         TEXT    NOT NULL,
    analyzer_version    TEXT    NOT NULL,
    metadata_json       TEXT,               -- analyzer-specific structured metadata (no secret content)
    -- freshness_state enum: CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE TABLE core_symbol_identities (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    language        TEXT    NOT NULL,
    qualified_name  TEXT    NOT NULL,
    -- kind enum: FUNCTION | CLASS | INTERFACE | CONSTANT | TYPE | MODULE | FIELD | ENUM | ENUM_VARIANT | MACRO | OTHER
    kind            TEXT    NOT NULL,
    disambiguator   TEXT    -- NULL if the (repo, language, qualified_name, kind) tuple is unambiguous
);

CREATE TABLE core_symbol_occurrences (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    symbol_identity_id  TEXT    NOT NULL REFERENCES core_symbol_identities(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- source_span format: "start_line:start_col-end_line:end_col" (1-based, inclusive)
    source_span         TEXT    NOT NULL,
    signature           TEXT,   -- language-specific signature string; NULL if unavailable
    visibility          TEXT,   -- language-specific visibility modifier; NULL if unavailable
    is_definition       INTEGER NOT NULL DEFAULT 0  -- BOOLEAN: 1 if this occurrence is the definition
);

CREATE TABLE core_workspace_catalog (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL UNIQUE REFERENCES core_repositories(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    -- JSON array of {"ecosystem": "...", "name": "..."} provided identities.
    provides_json       TEXT    NOT NULL DEFAULT '[]',
    -- BLAKE3 over sorted (path, content_hash) of declaration files; change
    -- detector for cheap incremental catalog refresh decisions.
    manifest_hash       TEXT    NOT NULL DEFAULT '',
    entry_count         INTEGER NOT NULL DEFAULT 0,
    -- CURRENT | STALE | INVALID | PENDING_REFRESH
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE core_workspace_snapshot_revisions (
    id                  TEXT    NOT NULL PRIMARY KEY,
    snapshot_id         TEXT    NOT NULL,
    repository_id       TEXT    NOT NULL,
    source_revision_id  TEXT    NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE TABLE core_workspace_snapshots (
    id              TEXT    NOT NULL PRIMARY KEY,
    created_at      INTEGER NOT NULL,
    repo_count      INTEGER NOT NULL DEFAULT 0,
    edges_emitted   INTEGER NOT NULL DEFAULT 0
);

CREATE VIRTUAL TABLE fts_retrieval_units USING fts5(
    retrieval_text,
    content='core_retrieval_units',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 1'
);

CREATE VIRTUAL TABLE fts_symbol_names USING fts5(
    qualified_name,
    kind,
    content='core_symbol_identities',
    content_rowid='rowid',
    tokenize='unicode61'
);

CREATE TABLE index_analysis_cache (
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    repo_relative   TEXT    NOT NULL,
    content_hash    TEXT    NOT NULL,
    security_state  TEXT    NOT NULL,
    is_partial_scan INTEGER NOT NULL,
    -- JSON-serialized Vec<PendingUnit>-equivalent retrieval units.
    units_json      TEXT    NOT NULL,
    -- JSON-serialized structural capture (Option<CapturedFile>); NULL when
    -- the file produced no structural intelligence.
    captured_json   TEXT,
    created_at      INTEGER NOT NULL,
    secret_pattern_version INTEGER NOT NULL DEFAULT 1,
    analyzer_registry_version TEXT NOT NULL DEFAULT '',
    discovery_policy_hash TEXT NOT NULL DEFAULT '',
    structural INTEGER NOT NULL DEFAULT 1,
    max_units_per_file INTEGER NOT NULL DEFAULT 512,
    PRIMARY KEY (repository_id, repo_relative)
);

CREATE TABLE ops_freshness_log (
    id          TEXT    NOT NULL PRIMARY KEY,   -- UUID
    entity_type TEXT    NOT NULL,   -- e.g., FILE_OCCURRENCE | RETRIEVAL_UNIT | EVIDENCE
    entity_id   TEXT    NOT NULL,
    prior_state TEXT    NOT NULL,
    new_state   TEXT    NOT NULL,
    changed_at  INTEGER NOT NULL,   -- microseconds since Unix epoch (UTC)
    reason      TEXT                -- human-readable reason; no secret content
);

CREATE TABLE ops_indexing_log (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID (= generation_id being built)
    generation_id   TEXT    NOT NULL,               -- correlates with core_index_generations.id
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    -- status enum: RUNNING | COMPLETED | ABANDONED | FAILED
    status          TEXT    NOT NULL DEFAULT 'RUNNING',
    files_total     INTEGER,
    files_processed INTEGER NOT NULL DEFAULT 0,
    started_at      INTEGER NOT NULL,
    completed_at    INTEGER,
    error_message   TEXT
);

CREATE TABLE ops_migration_log (
    id              TEXT    NOT NULL PRIMARY KEY,   -- migration id, e.g., "0001_initial"
    -- status enum: RUNNING | COMPLETED | FAILED
    status          TEXT    NOT NULL DEFAULT 'RUNNING',
    progress_json   TEXT,   -- JSON: { "last_completed_statement": N } for crash recovery
    started_at      INTEGER NOT NULL,
    completed_at    INTEGER,
    error_message   TEXT    -- populated if status = FAILED
);

CREATE TABLE ops_retrieval_log (
    plan_id         TEXT    NOT NULL PRIMARY KEY,   -- UUID
    query_id        TEXT    NOT NULL,               -- UUID; correlates with MCP tool call
    created_at_us   INTEGER NOT NULL,
    completed_at_us INTEGER,                        -- NULL if plan is still active (crash recovery marker)
    workspace_id    TEXT    NOT NULL,               -- SHA-256 hex of workspace root path
    -- query_type enum: DEFINITION_LOOKUP | SYMBOL_NAVIGATION | CONFIGURATION_LOOKUP |
    --                  ARCHITECTURE_EXPLANATION | DEBUGGING_ROOT_CAUSE | IMPACT_ANALYSIS |
    --                  CROSS_REPO_DEPENDENCY | KNOWLEDGE_QUESTION | TEST_BEHAVIOR | GENERIC_SEARCH
    query_type      TEXT    NOT NULL,
    -- result enum: SUCCESS | PARTIAL_SUCCESS | INSUFFICIENT_EVIDENCE | POLICY_HARD_CANCELLED |
    --              QUERY_TYPE_UNSUPPORTED | INTERNAL_ERROR
    result          TEXT    NOT NULL DEFAULT 'INTERNAL_ERROR',
    -- confidence enum: HIGH | MEDIUM | LOW | NONE
    confidence      TEXT    NOT NULL DEFAULT 'NONE',
    -- policy_mode enum: FAST | NORMAL | DEEP
    policy_mode     TEXT    NOT NULL,
    context_tokens  INTEGER NOT NULL DEFAULT 0,
    repair_cycles   INTEGER NOT NULL DEFAULT 0,
    plan_json       TEXT    NOT NULL               -- full serialized RetrievalPlan; no secret content
);

CREATE TABLE ops_server_state (
    id              TEXT    NOT NULL PRIMARY KEY DEFAULT 'singleton'
                            CHECK (id = 'singleton'),
    watcher_epoch   INTEGER NOT NULL DEFAULT 0,   -- incremented on each server startup
    schema_version  TEXT    NOT NULL,             -- matches current binary's expected version
    server_version  TEXT    NOT NULL,             -- Attic binary semver
    last_startup_at INTEGER NOT NULL,             -- microseconds since Unix epoch (UTC)
    last_shutdown_at INTEGER,                     -- NULL if last stop was a crash
    config_hash     TEXT    NOT NULL              -- SHA-256 hex of startup configuration
);

CREATE TABLE ops_tasks (
    id                  TEXT    NOT NULL PRIMARY KEY,   -- UUID
    repository_id       TEXT    REFERENCES core_repositories(id),  -- NULL for workspace-scope tasks
    -- task_type enum: FULL_INDEX | INCREMENTAL_INDEX | SEMANTIC_ENRICH | SECRET_SCAN |
    --                 STALE_EVICTION | LOG_PRUNING | INTEGRITY_CHECK | EMBEDDING_GENERATION
    task_type           TEXT    NOT NULL,
    priority            INTEGER NOT NULL DEFAULT 50,   -- higher = more urgent
    -- state enum: PENDING | RUNNING | DONE | FAILED | CANCELLED
    state               TEXT    NOT NULL DEFAULT 'PENDING',
    memory_budget_bytes INTEGER,   -- NULL = use class default from resources.md
    cpu_budget_ms       INTEGER,   -- NULL = use class default
    timeout_ms          INTEGER,   -- NULL = use class default
    checkpoint_json     TEXT,      -- partial progress for crash recovery; no secret content
    retry_count         INTEGER NOT NULL DEFAULT 0,
    max_retries         INTEGER NOT NULL DEFAULT 3,
    created_at          INTEGER NOT NULL,   -- microseconds since Unix epoch (UTC)
    started_at          INTEGER,
    completed_at        INTEGER,
    error_message       TEXT    -- last error if state = FAILED; no secret content
);

CREATE INDEX idx_evidence_freshness
    ON core_evidence(freshness_state)
    WHERE freshness_state IN ('STALE', 'INVALID');

CREATE INDEX idx_evidence_repo
    ON core_evidence(repository_id);

CREATE INDEX idx_evidence_revision
    ON core_evidence(source_revision_id);

CREATE INDEX idx_evidence_source
    ON core_evidence(source_id, source_type);

CREATE INDEX idx_file_identities_repo
    ON core_file_identities(repository_id);

CREATE INDEX idx_file_occ_content_hash
    ON core_file_occurrences(content_hash);

CREATE INDEX idx_file_occ_freshness
    ON core_file_occurrences(freshness_state)
    WHERE freshness_state IN ('STALE', 'INVALID', 'PENDING_REFRESH');

CREATE INDEX idx_file_occ_identity
    ON core_file_occurrences(file_identity_id);

CREATE INDEX idx_file_occ_path
    ON core_file_occurrences(path, source_revision_id);

CREATE INDEX idx_file_occ_revision
    ON core_file_occurrences(source_revision_id);

CREATE INDEX idx_file_occ_secret_scan
    ON core_file_occurrences(secret_scan_state)
    WHERE secret_scan_state = 'PENDING';

CREATE INDEX idx_freshness_log_entity
    ON ops_freshness_log(entity_id, entity_type, changed_at DESC);

CREATE INDEX idx_identity_links_from
    ON core_identity_links(from_identity_id);

CREATE INDEX idx_identity_links_repo
    ON core_identity_links(repository_id);

CREATE INDEX idx_identity_links_to
    ON core_identity_links(to_identity_id);

CREATE INDEX idx_index_analysis_cache_repo
    ON index_analysis_cache(repository_id);

CREATE INDEX idx_index_generations_revision
    ON core_index_generations(source_revision_id, created_at DESC);

CREATE INDEX idx_indexing_log_generation
    ON ops_indexing_log(generation_id);

CREATE INDEX idx_indexing_log_running
    ON ops_indexing_log(status)
    WHERE status = 'RUNNING';

CREATE INDEX idx_invalidation_artifact
    ON core_invalidation_records(artifact_type, artifact_id);

CREATE INDEX idx_invalidation_pending
    ON core_invalidation_records(recomputed_at)
    WHERE recomputed_at IS NULL;

CREATE INDEX idx_knowledge_items_file
    ON core_knowledge_items(file_occurrence_id);

CREATE INDEX idx_knowledge_items_repo
    ON core_knowledge_items(repository_id);

CREATE INDEX idx_relationships_cross_repo
    ON core_relationships(source_repository_id, target_repository_id)
    WHERE source_repository_id != target_repository_id;

CREATE INDEX idx_relationships_source
    ON core_relationships(source_entity_id, source_entity_type);

CREATE INDEX idx_relationships_target
    ON core_relationships(target_entity_id, target_entity_type);

CREATE INDEX idx_relationships_type
    ON core_relationships(rel_type);

CREATE INDEX idx_retrieval_log_incomplete
    ON ops_retrieval_log(completed_at_us)
    WHERE completed_at_us IS NULL;

CREATE INDEX idx_retrieval_log_result
    ON ops_retrieval_log(result);

CREATE INDEX idx_retrieval_log_workspace
    ON ops_retrieval_log(workspace_id, created_at_us DESC);

CREATE INDEX idx_retrieval_units_file
    ON core_retrieval_units(file_occurrence_id);

CREATE INDEX idx_retrieval_units_generation
    ON core_retrieval_units(index_generation_id);

CREATE INDEX idx_retrieval_units_repository
    ON core_retrieval_units(repository_id);

CREATE INDEX idx_retrieval_units_semantic_pending
    ON core_retrieval_units(semantic_state)
    WHERE semantic_state = 'PENDING';

CREATE INDEX idx_run_by_node
    ON core_retrieval_unit_nodes(structural_node_id);

CREATE INDEX idx_source_revisions_repo
    ON core_source_revisions(repository_id, captured_at DESC);

CREATE INDEX idx_structural_nodes_file
    ON core_structural_nodes(file_occurrence_id);

CREATE INDEX idx_structural_nodes_parent
    ON core_structural_nodes(parent_id);

CREATE INDEX idx_structural_nodes_stale
    ON core_structural_nodes(freshness_state)
    WHERE freshness_state IN ('STALE', 'INVALID');

CREATE INDEX idx_symbol_identity_name
    ON core_symbol_identities(qualified_name);

CREATE UNIQUE INDEX idx_symbol_identity_unique
    ON core_symbol_identities(repository_id, language, qualified_name, kind, COALESCE(disambiguator, ''));

CREATE INDEX idx_symbol_occ_definition
    ON core_symbol_occurrences(symbol_identity_id)
    WHERE is_definition = 1;

CREATE INDEX idx_symbol_occ_file
    ON core_symbol_occurrences(file_occurrence_id);

CREATE INDEX idx_symbol_occ_identity
    ON core_symbol_occurrences(symbol_identity_id);

CREATE INDEX idx_symbol_occ_revision
    ON core_symbol_occurrences(source_revision_id);

CREATE INDEX idx_tasks_pending_dedup
    ON ops_tasks(task_type, repository_id)
    WHERE state = 'PENDING';

CREATE INDEX idx_tasks_running
    ON ops_tasks(state)
    WHERE state = 'RUNNING';

CREATE INDEX idx_tasks_state_priority
    ON ops_tasks(state, priority DESC, created_at ASC)
    WHERE state = 'PENDING';

CREATE INDEX idx_tasks_type_state
    ON ops_tasks(task_type, state);

CREATE INDEX idx_ws_snapshot_revisions_snapshot
    ON core_workspace_snapshot_revisions(snapshot_id);

CREATE INDEX idx_xrepo_decls_name
    ON core_dependency_declarations(ecosystem, name);

CREATE INDEX idx_xrepo_decls_repo
    ON core_dependency_declarations(repository_id);

INSERT OR IGNORE INTO core_schema_migrations (id, applied_at) VALUES ('0001_initial', strftime('%s', 'now') * 1000000);

INSERT OR IGNORE INTO ops_migration_log (id, status, started_at, completed_at) VALUES ('0001_initial', 'COMPLETED', strftime('%s', 'now') * 1000000, strftime('%s', 'now') * 1000000);
