CREATE TABLE spec_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO spec_meta VALUES ('format', 'attic-spec-state'), ('created_by', '0.1.0');

CREATE TABLE spec_documents(
    spec_uid TEXT PRIMARY KEY,
    spec_id TEXT NOT NULL,
    owner_root_path TEXT NOT NULL,
    canonical_rel_path TEXT NOT NULL,
    title TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 1),
    state_version INTEGER NOT NULL CHECK(state_version >= 0),
    status TEXT NOT NULL CHECK(status IN ('UNDER_REVIEW','VERIFIED','APPROVED','IMPLEMENTING',
        'IMPLEMENTATION_INTERRUPTED','IMPLEMENTED','DRIFTED','ABANDONED')),
    status_reason TEXT,
    latest_run_id TEXT,
    latest_passing_run_id TEXT,
    verified_doc_sha256 TEXT,
    verified_fingerprint TEXT,
    verify_attempts INTEGER NOT NULL DEFAULT 0 CHECK(verify_attempts >= 0),
    attempts_doc_sha256 TEXT,
    approved_run_id TEXT,
    approved_doc_sha256 TEXT,
    approved_fingerprint TEXT,
    fencing_token INTEGER NOT NULL CHECK(fencing_token >= 0),
    lease_id TEXT,
    lease_expires_at INTEGER,
    lease_renewals INTEGER NOT NULL DEFAULT 0 CHECK(lease_renewals >= 0),
    implemented_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(owner_root_path,spec_id),
    UNIQUE(owner_root_path,canonical_rel_path),
    CHECK((status='IMPLEMENTING' AND lease_id IS NOT NULL AND lease_expires_at IS NOT NULL)
       OR (status<>'IMPLEMENTING' AND lease_id IS NULL AND lease_expires_at IS NULL))
);

CREATE TABLE spec_verification_runs(
    run_id TEXT PRIMARY KEY,
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    spec_revision INTEGER NOT NULL CHECK(spec_revision >= 1),
    run_kind TEXT NOT NULL CHECK(run_kind IN ('PRE_CODE','POST_CODE')),
    doc_sha256 TEXT NOT NULL,
    citation_fingerprint TEXT NOT NULL,
    revision_set_hash TEXT NOT NULL,
    source_receipts_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(source_receipts_json)),
    ast_verified_count INTEGER NOT NULL DEFAULT 0,
    file_only_count INTEGER NOT NULL DEFAULT 0,
    pre_create_ok_count INTEGER NOT NULL DEFAULT 0,
    dependency_ok_count INTEGER NOT NULL DEFAULT 0,
    unsupported_count INTEGER NOT NULL DEFAULT 0,
    warning_count INTEGER NOT NULL DEFAULT 0,
    error_count INTEGER NOT NULL DEFAULT 0,
    passed INTEGER NOT NULL CHECK(passed IN (0,1)),
    checked_at INTEGER NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
    CHECK((passed=1 AND error_count=0) OR (passed=0 AND error_count>0))
);
CREATE INDEX idx_spec_runs_spec ON spec_verification_runs(spec_uid,checked_at DESC);
CREATE TRIGGER run_no_update BEFORE UPDATE ON spec_verification_runs
BEGIN SELECT RAISE(ABORT,'verification runs are immutable'); END;
CREATE TABLE spec_citations(
    run_id TEXT NOT NULL REFERENCES spec_verification_runs(run_id) ON DELETE CASCADE,
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    citation_kind TEXT NOT NULL CHECK(citation_kind IN ('EXISTING_REF','TARGET_MODIFY',
        'TARGET_CREATE_FILE','TARGET_CREATE_SYMBOL','CROSS_REPO_DEPENDENCY','ALLOW_GLOB')),
    repository_id TEXT,
    dependency_repository_id TEXT,
    cited_path TEXT,
    cited_symbol TEXT,
    verification_tier TEXT NOT NULL CHECK(verification_tier IN ('AST_VERIFIED','FILE_ONLY_VERIFIED',
        'PRE_CREATE_ABSENT_OK','DEPENDENCY_VERIFIED','UNSUPPORTED_CONSTRUCT',
        'DEPENDENCY_UNVERIFIED','GLOB_ACCEPTED','MISSING_ERROR','COLLISION_ERROR')),
    index_generation_id TEXT,
    file_content_hash TEXT,
    symbol_signature_hash TEXT,
    start_line INTEGER,
    end_line INTEGER,
    message TEXT,
    payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
    PRIMARY KEY(run_id,ordinal)
);
CREATE TRIGGER citation_no_update BEFORE UPDATE ON spec_citations
BEGIN SELECT RAISE(ABORT,'citation evidence is immutable'); END;
CREATE TABLE spec_approvals(
    approval_id TEXT PRIMARY KEY,
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    spec_revision INTEGER NOT NULL,
    run_id TEXT NOT NULL REFERENCES spec_verification_runs(run_id),
    doc_sha256 TEXT NOT NULL,
    citation_fingerprint TEXT NOT NULL,
    approved_via TEXT NOT NULL CHECK(approved_via='CLI_TTY_CHALLENGE'),
    os_user TEXT,
    approved_at INTEGER NOT NULL,
    voided_at INTEGER,
    void_reason TEXT,
    CHECK((voided_at IS NULL AND void_reason IS NULL) OR
          (voided_at IS NOT NULL AND void_reason IS NOT NULL))
);
CREATE UNIQUE INDEX one_live_approval ON spec_approvals(spec_uid) WHERE voided_at IS NULL;
CREATE TRIGGER approval_run_matches_document BEFORE INSERT ON spec_approvals
WHEN NOT EXISTS (
    SELECT 1 FROM spec_verification_runs r
    WHERE r.run_id=NEW.run_id AND r.spec_uid=NEW.spec_uid AND r.spec_revision=NEW.spec_revision
      AND r.doc_sha256=NEW.doc_sha256 AND r.citation_fingerprint=NEW.citation_fingerprint
      AND r.passed=1 AND r.run_kind='PRE_CODE'
)
BEGIN SELECT RAISE(ABORT,'approval run must belong to this spec revision'); END;
CREATE TRIGGER approval_bindings_immutable BEFORE UPDATE ON spec_approvals
WHEN OLD.voided_at IS NOT NULL OR NEW.voided_at IS NULL OR NEW.void_reason IS NULL
  OR NEW.approval_id IS NOT OLD.approval_id OR NEW.spec_uid IS NOT OLD.spec_uid
  OR NEW.spec_revision IS NOT OLD.spec_revision OR NEW.run_id IS NOT OLD.run_id
  OR NEW.doc_sha256 IS NOT OLD.doc_sha256 OR NEW.citation_fingerprint IS NOT OLD.citation_fingerprint
  OR NEW.approved_via IS NOT OLD.approved_via OR NEW.os_user IS NOT OLD.os_user
  OR NEW.approved_at IS NOT OLD.approved_at
BEGIN SELECT RAISE(ABORT,'approval bindings are append-only'); END;
CREATE TRIGGER approval_no_delete BEFORE DELETE ON spec_approvals
BEGIN SELECT RAISE(ABORT,'approval history is append-only'); END;

CREATE TABLE spec_execution_waves(
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    spec_revision INTEGER NOT NULL,
    wave_number INTEGER NOT NULL CHECK(wave_number >= 1),
    repository_id TEXT NOT NULL,
    ordering_basis TEXT NOT NULL CHECK(ordering_basis IN ('UNLINKED_SINGLE_WAVE','MANIFEST_DEPENDENCY',
        'EXPLICIT_SPEC_DECLARATION','MANIFEST_DEPENDENCY_CYCLE')),
    allowed_paths_json TEXT NOT NULL CHECK(json_valid(allowed_paths_json)),
    allowed_globs_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(allowed_globs_json)),
    wave_status TEXT NOT NULL CHECK(wave_status IN ('PENDING','IN_PROGRESS','COMPLETED','SUPERSEDED')),
    started_at INTEGER,
    completed_at INTEGER,
    payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
    PRIMARY KEY(spec_uid,spec_revision,wave_number,repository_id)
);
CREATE TABLE spec_events(
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    op TEXT NOT NULL,
    actor TEXT NOT NULL CHECK(actor IN ('MCP','CLI','HOOK','RECONCILER')),
    from_status TEXT,
    to_status TEXT NOT NULL,
    fencing_token INTEGER NOT NULL,
    detail_json TEXT NOT NULL CHECK(json_valid(detail_json)),
    at INTEGER NOT NULL
);
CREATE TRIGGER event_no_update BEFORE UPDATE ON spec_events
BEGIN SELECT RAISE(ABORT,'events are append-only'); END;
CREATE TRIGGER event_no_delete BEFORE DELETE ON spec_events
BEGIN SELECT RAISE(ABORT,'events are append-only'); END;
CREATE TABLE spec_session_baselines(
    session_id TEXT NOT NULL,
    owner_root_path TEXT NOT NULL,
    captured_at INTEGER NOT NULL,
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    PRIMARY KEY(session_id,owner_root_path)
);
CREATE TABLE spec_content_receipts(
    owner_root_path TEXT NOT NULL,
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    spec_revision INTEGER NOT NULL,
    approved_run_id TEXT NOT NULL REFERENCES spec_verification_runs(run_id),
    path TEXT NOT NULL,
    operation TEXT NOT NULL,
    file_mode TEXT NOT NULL,
    blob_id TEXT NOT NULL,
    PRIMARY KEY(owner_root_path,spec_uid,spec_revision,path,operation,file_mode,blob_id)
);
