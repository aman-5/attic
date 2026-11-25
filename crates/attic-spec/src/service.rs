use crate::{
    Result, SpecError, citations, now_micros, sha256,
    state::{self, Document, Operation, Status},
    store::{self, SpecStore},
    verifier::{self, Repository, Run},
    waves::{self, Wave},
};
use attic_core::config::PlanningConfig;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub trait Clock: Send + Sync {
    fn now(&self) -> Result<i64>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Result<i64> {
        now_micros()
    }
}

pub trait ApprovalPrompt {
    fn is_interactive(&self) -> bool;
    fn confirm(&mut self, doc: &Document, run: &Run, code: &str) -> Result<bool>;
}

pub struct SpecService {
    pub store: SpecStore,
    pub config: PlanningConfig,
    pub data_dir: PathBuf,
    pub packs: Arc<tokio::sync::Semaphore>,
    pub clock: Arc<dyn Clock>,
}

impl SpecService {
    pub fn open(catalog: &Path, config: PlanningConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        config
            .validate()
            .map_err(|e| SpecError::new("ERR_CONFIG", e.to_string()))?;
        let store = SpecStore::open(&SpecStore::path_for_catalog(catalog))?;
        Ok(Some(Self {
            store,
            packs: Arc::new(tokio::sync::Semaphore::new(config.packs.max_concurrent)),
            config,
            data_dir: catalog
                .parent()
                .ok_or_else(|| SpecError::new("ERR_CONFIG", "catalog has no parent"))?
                .into(),
            clock: Arc::new(SystemClock),
        }))
    }

    pub fn resolve(&self, id: &str, root: Option<&str>) -> Result<Document> {
        let found: Vec<_> = self
            .store
            .documents()?
            .into_iter()
            .filter(|d| d.spec_id == id && root.is_none_or(|r| d.owner_root_path == r))
            .collect();
        match found.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(SpecError::new("ERR_SPEC_NOT_FOUND", "spec not registered")),
            _ => Err(SpecError::new(
                "ERR_AMBIGUOUS_SPEC_ID",
                "pass root to disambiguate spec id",
            )),
        }
    }
    pub fn doc_bytes(doc: &Document) -> Result<Vec<u8>> {
        let path = verifier::safe_path(Path::new(&doc.owner_root_path), &doc.canonical_rel_path)?;
        let meta = std::fs::metadata(&path)
            .map_err(|_| SpecError::new("ERR_SPEC_DOC_MISSING", "spec document missing"))?;
        if meta.len() > 1_048_576 {
            return Err(SpecError::new("ERR_CITATION_PARSE", "doc too large"));
        }
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(1_048_577)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 1_048_576 {
            return Err(SpecError::new(
                "ERR_CITATION_PARSE",
                "doc grew beyond size limit",
            ));
        }
        Ok(bytes)
    }
    pub fn effective(&self, doc: &Document) -> Result<Document> {
        state::reconcile(doc, &Self::observed_doc_hash(doc), self.clock.now()?)
    }
    pub fn reconciled(&self, doc: &Document) -> Result<Document> {
        self.store
            .reconcile(doc, &Self::observed_doc_hash(doc), self.clock.now()?)
    }
    fn observed_doc_hash(doc: &Document) -> String {
        match Self::doc_bytes(doc) {
            Ok(bytes) => sha256(&bytes),
            Err(error) => {
                tracing::warn!(code=%error.code, "spec document unavailable; invalidating evidence");
                // Empty is not a SHA-256 digest: unavailable bytes never prove approval.
                String::new()
            }
        }
    }
    pub fn register(&self, id: &str, repo: &Repository) -> Result<Document> {
        if !citations::valid_id(id) {
            return Err(SpecError::new("ERR_INVALID_SPEC_ID", "invalid spec id"));
        }
        let root = verifier::identity(&repo.root);
        let path = format!("{}/{id}.md", self.config.designs_dir);
        let mut doc = Document::new(id, &root, &path, id, self.clock.now()?);
        let bytes = Self::doc_bytes(&doc)?;
        citations::parse(&bytes)?;
        doc.title = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("# ")))
            .unwrap_or(id)
            .to_string();
        match self.resolve(id, Some(&root)) {
            Ok(existing) => {
                let existing = self.reconciled(&existing)?;
                self.store
                    .reregister(&existing, &doc.title, self.clock.now()?)
            }
            Err(e) if e.code == "ERR_SPEC_NOT_FOUND" => {
                self.store.write(|tx| store::insert(tx, &doc))?;
                Ok(doc)
            }
            Err(e) => Err(e),
        }
    }
    pub fn status(&self, doc: &Document) -> Result<Value> {
        let effective = self.effective(doc)?;
        let review = if effective.status == Status::Verified {
            match self.review_prepare(&effective) {
                Ok(value) => value,
                Err(error) => json!({"status":"review_pending","error":error}),
            }
        } else {
            json!({"status":"not_applicable"})
        };
        let mut next = self.next_action(&effective);
        if effective.status == Status::Verified && review["status"] != "review_passed_host_attested"
        {
            let actor = if self.config.mode == attic_core::config::PlanningMode::Manual {
                "user"
            } else {
                "agent"
            };
            next = json!({"action":"review","actor":actor,
                "message":"Complete two clean host-attested review rounds before requesting user approval."});
        }
        Ok(
            json!({"spec_id":doc.spec_id,"status":effective.status,"persisted_status":doc.status,
            "revision":doc.revision,"document":effective,"waves":self.waves(doc)?,
            "latest_run":doc.latest_run_id.as_ref().map(|id|self.run(id)).transpose()?,
            "review":review,"next_required_action":next}),
        )
    }
    pub fn next_action(&self, doc: &Document) -> Value {
        let (action, actor, message) = match doc.status {
            Status::UnderReview => (
                "verify",
                "agent",
                "verify citations; edit or reopen the document if required",
            ),
            Status::Verified => (
                "approve",
                "user",
                "Run attic spec approve in your own terminal. The agent must never run approval.",
            ),
            Status::Approved => ("begin", "agent", "begin a fresh, fenced implementation"),
            Status::Implementing => (
                "wave_complete",
                "agent",
                "complete current waves, heartbeat before expiry, then drift-check",
            ),
            Status::ImplementationInterrupted => (
                "resume",
                "agent",
                "resume to acquire a new lease; old workers must stop",
            ),
            Status::Implemented => (
                "review",
                "user",
                "drift proves citations, not tests or feature correctness",
            ),
            Status::Drifted => (
                "reopen",
                "user",
                "reopen changed/incomplete scope and obtain renewed approval",
            ),
            Status::Abandoned => (
                "register",
                "user",
                "register only when a new revision is intended",
            ),
        };
        json!({"action":action,"actor":if actor=="agent"&&self.config.mode==attic_core::config::PlanningMode::Manual{"user"}else{actor},"message":message})
    }
    pub fn run(&self, id: &str) -> Result<Run> {
        self.store.read(|conn| {
            let payload: String = conn.query_row(
                "SELECT payload FROM spec_verification_runs WHERE run_id=?1",
                [id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str(&payload)?)
        })
    }
    pub fn waves(&self, doc: &Document) -> Result<Vec<Wave>> {
        self.store.read(|conn| load_waves(conn, doc))
    }
    pub fn verify(
        &self,
        expected: &Document,
        post: bool,
        repos: &[Repository],
        edges: &[(String, String, String)],
        lease: Option<(&str, i64)>,
        begin: bool,
    ) -> Result<Document> {
        let doc = self.reconciled(expected)?;
        if begin && expected.status == Status::Approved && doc.status != Status::Approved {
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "document reconciliation revoked approval before begin",
            ));
        }
        let bytes = Self::doc_bytes(&doc)?;
        let hash = sha256(&bytes);
        let reviewed_run = if begin {
            let run = self.run(doc.approved_run_id.as_deref().ok_or_else(|| {
                SpecError::new("ERR_APPROVAL_STALE", "approved run is missing")
            })?)?;
            self.store
                .read(|conn| self.require_review_binding(conn, &doc, &run))?;
            Some(run)
        } else {
            None
        };
        if begin {
            state::transition(doc.status, Operation::Begin)?;
        } else if post {
            state::transition(doc.status, Operation::DriftPass)?;
            if doc.approved_doc_sha256.as_deref() != Some(&hash) {
                return Err(SpecError::new(
                    "ERR_APPROVAL_STALE",
                    "document changed; reopen and obtain approval",
                ));
            }
            if doc.status == Status::Implementing {
                let (id, fence) = lease.ok_or_else(|| {
                    SpecError::new("ERR_STALE_FENCING_TOKEN", "lease and fence required")
                })?;
                doc.check_fence(id, fence)?;
            }
            if self.waves(&doc)?.is_empty()
                || self.waves(&doc)?.iter().any(|w| w.status != "COMPLETED")
            {
                return Err(SpecError::new(
                    "ERR_WAVES_INCOMPLETE",
                    "complete current revision waves before drift",
                ));
            }
        } else {
            state::transition(doc.status, Operation::VerifyPass)?;
            if doc.attempts_doc_sha256.as_deref() == Some(&hash)
                && doc.verify_attempts >= self.config.validation.max_verify_attempts
            {
                return Err(SpecError::new(
                    "ERR_VERIFY_ATTEMPTS_EXHAUSTED",
                    "edit the spec or review failures with the user",
                ));
            }
        }
        let run = verifier::verify(
            &bytes,
            doc.revision,
            post,
            repos,
            edges,
            &self.config.validation,
        )?;
        let current = self.reconciled(&doc)?;
        if current.state_version != doc.state_version {
            return Err(SpecError::new(
                "ERR_STALE_FENCING_TOKEN",
                "lease or document changed during verification",
            ));
        }
        if sha256(&Self::doc_bytes(&doc)?) != hash {
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "document moved during verification",
            ));
        }
        let stale = begin
            && (!run.passed
                || doc.approved_doc_sha256.as_deref() != Some(&run.doc_sha256)
                || doc.approved_fingerprint.as_deref() != Some(&run.citation_fingerprint)
                || reviewed_run
                    .as_ref()
                    .map(crate::review::evidence_hash)
                    .transpose()?
                    != Some(crate::review::evidence_hash(&run)?));
        let changed_approval = doc.status == Status::Approved
            && (!run.passed
                || doc.approved_doc_sha256.as_deref() != Some(&run.doc_sha256)
                || doc.approved_fingerprint.as_deref() != Some(&run.citation_fingerprint));
        let mut next = doc.clone();
        next.latest_run_id = Some(run.run_id.clone());
        if !post && run.passed {
            next.latest_passing_run_id = Some(run.run_id.clone());
            next.verified_doc_sha256 = Some(hash.clone());
            next.verified_fingerprint = Some(run.citation_fingerprint.clone());
        }
        if post {
            next.leave_implementation(if run.passed {
                Status::Implemented
            } else {
                Status::Drifted
            })?;
            if run.passed {
                next.implemented_at = Some(self.clock.now()?);
            }
        } else if begin && !stale {
            next.status = Status::Implementing;
            self.new_lease(&mut next)?;
        } else {
            next.status = if !run.passed || changed_approval {
                Status::UnderReview
            } else {
                state::transition(doc.status, Operation::VerifyPass)?
            };
            next.verify_attempts = if run.passed {
                0
            } else if doc.attempts_doc_sha256.as_deref() == Some(&hash) {
                doc.verify_attempts.saturating_add(1)
            } else {
                1
            };
            next.attempts_doc_sha256 = Some(hash);
        }
        if stale || changed_approval {
            next.status = Status::UnderReview;
            next.approved_run_id = None;
            next.approved_doc_sha256 = None;
            next.approved_fingerprint = None;
            next.status_reason = Some("APPROVAL_STALE".into());
        }
        let planned = if begin && !stale {
            waves::plan(&run.citations, edges)?
        } else {
            Vec::new()
        };
        let receipts = if post && run.passed {
            crate::boundary::completion_receipts(self, &doc, &run)?
        } else {
            Vec::new()
        };
        if sha256(&Self::doc_bytes(&doc)?) != run.doc_sha256 {
            self.reconciled(&doc)?;
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "document moved before verification persistence",
            ));
        }
        self.lease_write(&doc,|tx| {
            if let Some(reviewed) = &reviewed_run {
                if stale {
                    self.require_review_binding(tx, &doc, reviewed)?;
                } else {
                    self.require_review(tx, &doc, reviewed)?;
                }
            }
            persist_run(tx,&doc,&run)?;
            if stale||changed_approval{store::void_approval(tx,&doc.spec_uid,"APPROVAL_STALE",self.clock.now()?)?;}
            if begin&&!stale{for wave in &planned {
                tx.execute("INSERT INTO spec_execution_waves(spec_uid,spec_revision,wave_number,repository_id,ordering_basis,allowed_paths_json,allowed_globs_json,wave_status,payload) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![doc.spec_uid,doc.revision,wave.wave_number,wave.repository_id,wave.ordering_basis,serde_json::to_string(&wave.allowed_paths)?,serde_json::to_string(&wave.allowed_globs)?,wave.status,serde_json::to_string(wave)?])?;
            }}
            for receipt in &receipts {
                tx.execute("INSERT OR IGNORE INTO spec_content_receipts(owner_root_path,spec_uid,spec_revision,approved_run_id,path,operation,file_mode,blob_id)
                    VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                    params![receipt["root"].as_str(),doc.spec_uid,doc.revision,doc.approved_run_id,
                        receipt["path"].as_str(),receipt["operation"].as_str(),receipt["mode"].as_str(),receipt["blob"].as_str()])?;
            }
            store::save(tx,&doc,&mut next,if begin{"begin"}else if post{"drift"}else{"verify"},"MCP",&json!({"run_id":run.run_id}),self.clock.now()?)?;
            store::retain_runs(tx,&next,self.config.validation.max_runs_retained)?;
            Ok(())
        })?;
        if stale {
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "fresh verification differs from the approved run; obtain renewed approval",
            ));
        }
        Ok(next)
    }
    fn new_lease(&self, doc: &mut Document) -> Result<()> {
        doc.fencing_token = doc
            .fencing_token
            .checked_add(1)
            .ok_or_else(|| SpecError::new("ERR_FENCE_EXHAUSTED", "counter exhausted"))?;
        doc.lease_id = Some(uuid::Uuid::new_v4().to_string());
        doc.lease_expires_at = Some(self.expiry()?);
        doc.lease_renewals = 0;
        Ok(())
    }

    fn lease_write<T>(
        &self,
        doc: &Document,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let result = self.store.write(|tx| {
            let now = self.clock.now()?;
            if doc.status == Status::Implementing
                && doc.lease_expires_at.is_none_or(|expires| now >= expires)
            {
                return Err(SpecError::new(
                    "ERR_LEASE_EXPIRED",
                    "lease expired before the write; resume with a new fence",
                ));
            }
            operation(tx)
        });
        if result
            .as_ref()
            .is_err_and(|e| e.code == "ERR_LEASE_EXPIRED")
        {
            self.store.reconcile(
                doc,
                doc.approved_doc_sha256.as_deref().unwrap_or(""),
                self.clock.now()?,
            )?;
        }
        result
    }
    fn expiry(&self) -> Result<i64> {
        self.clock
            .now()?
            .checked_add(
                i64::try_from(self.config.lease.lease_seconds)
                    .map_err(|_| SpecError::new("ERR_CONFIG", "lease overflow"))?
                    .checked_mul(1_000_000)
                    .ok_or_else(|| SpecError::new("ERR_CONFIG", "lease overflow"))?,
            )
            .ok_or_else(|| SpecError::new("ERR_CLOCK", "lease expiry overflow"))
    }
    pub fn heartbeat(&self, expected: &Document, id: &str, fence: i64) -> Result<Document> {
        let doc = self.reconciled(expected)?;
        doc.check_fence(id, fence)?;
        if doc.lease_renewals >= self.config.lease.max_renewals {
            return Err(SpecError::new(
                "ERR_LEASE_RENEWAL_LIMIT",
                "renewal budget exhausted",
            ));
        }
        let mut next = doc.clone();
        next.lease_expires_at = Some(self.expiry()?);
        next.lease_renewals += 1;
        self.lease_write(&doc, |tx| {
            store::save(
                tx,
                &doc,
                &mut next,
                "heartbeat",
                "MCP",
                &json!({}),
                self.clock.now()?,
            )
        })?;
        Ok(next)
    }
    pub fn resume(&self, expected: &Document) -> Result<Document> {
        let doc = self.reconciled(expected)?;
        state::transition(doc.status, Operation::Resume)?;
        let mut next = doc.clone();
        if doc.approved_doc_sha256.as_deref() != Some(&sha256(&Self::doc_bytes(&doc)?)) {
            next.leave_implementation(Status::Drifted)?;
            next.status_reason = Some("DOC_CHANGED_DURING_IMPLEMENTATION".into());
        } else {
            next.status = Status::Implementing;
            self.new_lease(&mut next)?;
        }
        self.store.write(|tx| {
            store::save(
                tx,
                &doc,
                &mut next,
                "resume",
                "MCP",
                &json!({}),
                self.clock.now()?,
            )
        })?;
        Ok(next)
    }
    pub fn wave_complete(
        &self,
        expected: &Document,
        id: &str,
        fence: i64,
        number: u32,
        repository: &str,
    ) -> Result<Document> {
        let doc = self.reconciled(expected)?;
        doc.check_fence(id, fence)?;
        let mut next = doc.clone();
        self.lease_write(&doc,|tx|{
            let n=tx.execute("UPDATE spec_execution_waves SET wave_status='COMPLETED',completed_at=?5 WHERE spec_uid=?1 AND spec_revision=?2 AND wave_number=?3 AND repository_id=?4 AND wave_status='IN_PROGRESS'",
                params![doc.spec_uid,doc.revision,number,repository,self.clock.now()?])?;
            if n!=1{return Err(SpecError::new("ERR_WAVE_NOT_IN_PROGRESS","wave is not current and in progress"));}
            let remaining:i64=tx.query_row("SELECT COUNT(*) FROM spec_execution_waves WHERE spec_uid=?1 AND spec_revision=?2 AND wave_number=?3 AND wave_status<>'COMPLETED'",params![doc.spec_uid,doc.revision,number],|r|r.get(0))?;
            if remaining==0 {tx.execute("UPDATE spec_execution_waves SET wave_status='IN_PROGRESS',started_at=?4 WHERE spec_uid=?1 AND spec_revision=?2 AND wave_number=?3 AND wave_status='PENDING'",params![doc.spec_uid,doc.revision,number+1,self.clock.now()?])?;}
            store::save(tx,&doc,&mut next,"wave_complete","MCP",&json!({"wave":number,"repository":repository}),self.clock.now()?)
        })?;
        Ok(next)
    }
    pub fn approve(
        &self,
        expected: &Document,
        prompt: &mut dyn ApprovalPrompt,
    ) -> Result<Document> {
        if !prompt.is_interactive() {
            return Err(SpecError::new(
                "ERR_APPROVAL_REQUIRES_TTY",
                "approval requires stdin/stdout/stderr terminals",
            ));
        }
        let doc = self.reconciled(expected)?;
        state::transition(doc.status, Operation::Approve)?;
        let run = self.run(
            doc.latest_run_id
                .as_deref()
                .ok_or_else(|| SpecError::new("ERR_APPROVAL_STALE", "no verified run"))?,
        )?;
        if !run.passed
            || run.kind != "PRE_CODE"
            || doc.verified_doc_sha256.as_deref() != Some(&run.doc_sha256)
            || doc.verified_fingerprint.as_deref() != Some(&run.citation_fingerprint)
            || doc.latest_run_id != doc.latest_passing_run_id
            || run.spec_revision != doc.revision
            || sha256(&Self::doc_bytes(&doc)?) != run.doc_sha256
        {
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "document/run changed since verification",
            ));
        }
        self.store
            .read(|conn| self.require_review(conn, &doc, &run))?;
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let random = uuid::Uuid::new_v4();
        let code: String = random.as_bytes()[..6]
            .iter()
            .map(|b| ALPHABET[usize::from(*b) % ALPHABET.len()] as char)
            .collect();
        if !prompt.confirm(&doc, &run, &code)? {
            return Err(SpecError::new(
                "ERR_APPROVAL_CANCELLED",
                "challenge cancelled",
            ));
        }
        if sha256(&Self::doc_bytes(&doc)?) != run.doc_sha256 {
            self.reconciled(&doc)?;
            return Err(SpecError::new(
                "ERR_APPROVAL_STALE",
                "document changed while prompting",
            ));
        }
        let mut next = doc.clone();
        next.status = Status::Approved;
        next.approved_run_id = Some(run.run_id.clone());
        next.approved_doc_sha256 = Some(run.doc_sha256.clone());
        next.approved_fingerprint = Some(run.citation_fingerprint.clone());
        self.store.write(|tx|{
            self.require_review(tx,&doc,&run)?;
            store::save(tx,&doc,&mut next,"approve","CLI",&json!({"run_id":run.run_id}),self.clock.now()?)?;
            tx.execute("INSERT INTO spec_approvals(approval_id,spec_uid,spec_revision,run_id,doc_sha256,citation_fingerprint,approved_via,os_user,approved_at)
                VALUES(?1,?2,?3,?4,?5,?6,'CLI_TTY_CHALLENGE',?7,?8)",params![uuid::Uuid::new_v4().to_string(),doc.spec_uid,doc.revision,run.run_id,run.doc_sha256,run.citation_fingerprint,
                    std::env::var("USER").or_else(|_|std::env::var("USERNAME")).ok(),self.clock.now()?])?;Ok(())
        })?;
        Ok(next)
    }
}

pub fn load_waves(conn: &Connection, doc: &Document) -> Result<Vec<Wave>> {
    let mut q=conn.prepare("SELECT payload,wave_status FROM spec_execution_waves WHERE spec_uid=?1 AND spec_revision=?2 ORDER BY wave_number,repository_id")?;
    let rows = q.query_map(params![doc.spec_uid, doc.revision], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (payload, status) = row?;
        let mut w: Wave = serde_json::from_str(&payload)?;
        w.status = status;
        Ok(w)
    })
    .collect()
}

fn persist_run(tx: &rusqlite::Transaction<'_>, doc: &Document, run: &Run) -> Result<()> {
    let count = |tier: &str| run.citations.iter().filter(|f| f.tier == tier).count() as i64;
    tx.execute("INSERT INTO spec_verification_runs(run_id,spec_uid,spec_revision,run_kind,doc_sha256,citation_fingerprint,revision_set_hash,source_receipts_json,ast_verified_count,file_only_count,pre_create_ok_count,dependency_ok_count,unsupported_count,warning_count,error_count,passed,checked_at,payload)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
        params![run.run_id,doc.spec_uid,doc.revision,run.kind,run.doc_sha256,run.citation_fingerprint,run.revision_set_hash,serde_json::to_string(&run.source_receipts)?,
            count("AST_VERIFIED"),count("FILE_ONLY_VERIFIED"),count("PRE_CREATE_ABSENT_OK"),count("DEPENDENCY_VERIFIED"),count("UNSUPPORTED_CONSTRUCT"),
            run.citations.iter().filter(|f|f.warning).count() as i64,run.citations.iter().filter(|f|f.error).count() as i64,run.passed,run.checked_at,serde_json::to_string(run)?])?;
    for f in &run.citations {
        tx.execute("INSERT INTO spec_citations(run_id,ordinal,citation_kind,repository_id,dependency_repository_id,cited_path,cited_symbol,verification_tier,index_generation_id,file_content_hash,symbol_signature_hash,start_line,end_line,message,payload)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![run.run_id,f.ordinal as i64,f.citation.kind.as_str(),f.repository_id,f.dependency_repository_id,f.citation.path,f.citation.symbol,f.tier,f.generation,f.content_hash,f.signature_hash,f.start_line,f.end_line,f.message,serde_json::to_string(f)?])?;
    }
    Ok(())
}
