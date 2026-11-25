CREATE TABLE spec_review_attempts(
    attempt_id TEXT PRIMARY KEY,
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    created_at INTEGER NOT NULL
);
CREATE INDEX idx_review_attempts_spec ON spec_review_attempts(spec_uid, created_at, attempt_id);
CREATE TABLE spec_review_rounds(
    attempt_id TEXT NOT NULL REFERENCES spec_review_attempts(attempt_id),
    round INTEGER NOT NULL CHECK(round > 0),
    spec_uid TEXT NOT NULL REFERENCES spec_documents(spec_uid),
    spec_revision INTEGER NOT NULL,
    run_id TEXT NOT NULL REFERENCES spec_verification_runs(run_id),
    doc_sha256 TEXT NOT NULL,
    evidence_sha256 TEXT NOT NULL,
    clean INTEGER NOT NULL CHECK(clean IN (0,1)),
    payload TEXT NOT NULL CHECK(json_valid(payload)),
    created_at INTEGER NOT NULL,
    PRIMARY KEY(attempt_id, round)
);
CREATE INDEX idx_review_rounds_spec ON spec_review_rounds(spec_uid, run_id);
CREATE TRIGGER review_round_binding BEFORE INSERT ON spec_review_rounds
WHEN NEW.round <> COALESCE(
        (SELECT MAX(round) FROM spec_review_rounds WHERE attempt_id=NEW.attempt_id), 0
    ) + 1
 OR NOT EXISTS (
    SELECT 1 FROM spec_review_attempts a
    JOIN spec_documents d ON d.spec_uid=a.spec_uid
    JOIN spec_verification_runs r ON r.run_id=NEW.run_id AND r.spec_uid=d.spec_uid
    WHERE a.attempt_id=NEW.attempt_id AND d.spec_uid=NEW.spec_uid
      AND d.status='VERIFIED' AND d.revision=NEW.spec_revision
      AND d.latest_run_id=NEW.run_id AND d.latest_passing_run_id=NEW.run_id
      AND d.verified_doc_sha256=NEW.doc_sha256 AND r.spec_revision=NEW.spec_revision
      AND r.doc_sha256=NEW.doc_sha256 AND r.run_kind='PRE_CODE' AND r.passed=1
)
BEGIN SELECT RAISE(ABORT,'review round must match the current verified run and sequence'); END;
CREATE TRIGGER review_attempt_no_update BEFORE UPDATE ON spec_review_attempts
BEGIN SELECT RAISE(ABORT,'review attempts are immutable'); END;
CREATE TRIGGER review_attempt_no_delete BEFORE DELETE ON spec_review_attempts
BEGIN SELECT RAISE(ABORT,'review attempts are immutable'); END;
CREATE TRIGGER review_round_no_update BEFORE UPDATE ON spec_review_rounds
BEGIN SELECT RAISE(ABORT,'review rounds are immutable'); END;
CREATE TRIGGER review_round_no_delete BEFORE DELETE ON spec_review_rounds
BEGIN SELECT RAISE(ABORT,'review rounds are immutable'); END;
