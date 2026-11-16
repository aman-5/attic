use serde::{Deserialize, Serialize};

use crate::{Result, SpecError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    UnderReview,
    Verified,
    Approved,
    Implementing,
    ImplementationInterrupted,
    Implemented,
    Drifted,
    Abandoned,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnderReview => "UNDER_REVIEW",
            Self::Verified => "VERIFIED",
            Self::Approved => "APPROVED",
            Self::Implementing => "IMPLEMENTING",
            Self::ImplementationInterrupted => "IMPLEMENTATION_INTERRUPTED",
            Self::Implemented => "IMPLEMENTED",
            Self::Drifted => "DRIFTED",
            Self::Abandoned => "ABANDONED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Document {
    pub spec_uid: String,
    pub spec_id: String,
    pub owner_root_path: String,
    pub canonical_rel_path: String,
    pub title: String,
    pub revision: i64,
    pub state_version: i64,
    pub status: Status,
    pub status_reason: Option<String>,
    pub latest_run_id: Option<String>,
    pub latest_passing_run_id: Option<String>,
    pub verified_doc_sha256: Option<String>,
    pub verified_fingerprint: Option<String>,
    pub verify_attempts: u32,
    pub attempts_doc_sha256: Option<String>,
    pub approved_run_id: Option<String>,
    pub approved_doc_sha256: Option<String>,
    pub approved_fingerprint: Option<String>,
    pub fencing_token: i64,
    pub lease_id: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub lease_renewals: u32,
    pub implemented_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Document {
    pub fn new(spec_id: &str, root: &str, path: &str, title: &str, now: i64) -> Self {
        Self {
            spec_uid: uuid::Uuid::new_v4().to_string(),
            spec_id: spec_id.into(),
            owner_root_path: root.into(),
            canonical_rel_path: path.into(),
            title: title.into(),
            revision: 1,
            state_version: 0,
            status: Status::UnderReview,
            status_reason: None,
            latest_run_id: None,
            latest_passing_run_id: None,
            verified_doc_sha256: None,
            verified_fingerprint: None,
            verify_attempts: 0,
            attempts_doc_sha256: None,
            approved_run_id: None,
            approved_doc_sha256: None,
            approved_fingerprint: None,
            fencing_token: 0,
            lease_id: None,
            lease_expires_at: None,
            lease_renewals: 0,
            implemented_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn leave_implementation(&mut self, to: Status) -> Result<()> {
        self.status = to;
        self.lease_id = None;
        self.lease_expires_at = None;
        self.lease_renewals = 0;
        self.fencing_token = self
            .fencing_token
            .checked_add(1)
            .ok_or_else(|| SpecError::new("ERR_FENCE_EXHAUSTED", "fencing counter exhausted"))?;
        Ok(())
    }

    pub fn clear_verification(&mut self) {
        self.latest_run_id = None;
        self.latest_passing_run_id = None;
        self.verified_doc_sha256 = None;
        self.verified_fingerprint = None;
        self.verify_attempts = 0;
        self.attempts_doc_sha256 = None;
        self.approved_run_id = None;
        self.approved_doc_sha256 = None;
        self.approved_fingerprint = None;
        self.implemented_at = None;
    }

    pub fn check_fence(&self, lease: &str, fence: i64) -> Result<()> {
        if self.status == Status::ImplementationInterrupted {
            return Err(SpecError::new(
                "ERR_LEASE_EXPIRED",
                "resume with a new lease",
            ));
        }
        if self.status != Status::Implementing
            || self.lease_id.as_deref() != Some(lease)
            || self.fencing_token != fence
        {
            return Err(SpecError::new(
                "ERR_STALE_FENCING_TOKEN",
                "lease or fencing token no longer current",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Register,
    VerifyPass,
    VerifyFail,
    Approve,
    Begin,
    Heartbeat,
    WaveComplete,
    Resume,
    DriftPass,
    DriftFail,
    Reopen,
    Abandon,
}

/// Pure transition admission. Evidence-specific guards belong to the service.
pub fn transition(from: Status, op: Operation) -> Result<Status> {
    use Operation::*;
    use Status::*;
    let to = match (from, op) {
        (Abandoned, Register) => UnderReview,
        (_, Register) => from,
        (UnderReview | Verified, VerifyPass) => Verified,
        (Approved, VerifyPass) => Approved,
        (UnderReview | Verified | Approved, VerifyFail) => UnderReview,
        (Verified, Approve) => Approved,
        (Approved, Begin) | (ImplementationInterrupted, Resume) => Implementing,
        (Implementing, Heartbeat | WaveComplete) => Implementing,
        (Implementing | Implemented | Drifted, DriftPass) => Implemented,
        (Implementing | Implemented | Drifted, DriftFail) => Drifted,
        (
            UnderReview
            | Approved
            | Implementing
            | ImplementationInterrupted
            | Implemented
            | Drifted,
            Reopen,
        ) => UnderReview,
        (_, Abandon) => Abandoned,
        _ => {
            return Err(SpecError::new(
                "ERR_INVALID_STATE_TRANSITION",
                format!("cannot {op:?} from {}", from.as_str()),
            ));
        }
    };
    Ok(to)
}

/// Read-only effective state projection. Persist this separately before a mutation.
pub fn reconcile(doc: &Document, doc_hash: &str, now: i64) -> Result<Document> {
    let mut next = doc.clone();
    if doc_hash.is_empty() {
        match doc.status {
            Status::Implementing => next.leave_implementation(Status::ImplementationInterrupted)?,
            Status::Approved | Status::Verified => next.status = Status::UnderReview,
            Status::Implemented => next.status = Status::Drifted,
            _ => {}
        }
        next.verified_doc_sha256 = None;
        next.verified_fingerprint = None;
        next.approved_run_id = None;
        next.approved_doc_sha256 = None;
        next.approved_fingerprint = None;
        next.status_reason = Some("DOC_UNAVAILABLE".into());
        return Ok(next);
    }
    let approved_changed = doc
        .approved_doc_sha256
        .as_deref()
        .is_some_and(|h| h != doc_hash);
    match doc.status {
        Status::Implementing
            if approved_changed || doc.lease_expires_at.is_none_or(|t| now >= t) =>
        {
            next.leave_implementation(Status::ImplementationInterrupted)?;
            next.status_reason = Some(
                if approved_changed {
                    "DOC_CHANGED"
                } else {
                    "LEASE_EXPIRED"
                }
                .into(),
            );
        }
        Status::Verified if doc.verified_doc_sha256.as_deref() != Some(doc_hash) => {
            next.status = Status::UnderReview;
            next.status_reason = Some("DOC_CHANGED".into());
        }
        Status::Approved if approved_changed => {
            next.status = Status::UnderReview;
            next.status_reason = Some("DOC_CHANGED".into());
            next.approved_run_id = None;
            next.approved_doc_sha256 = None;
            next.approved_fingerprint = None;
        }
        _ => {}
    }
    Ok(next)
}
