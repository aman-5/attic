//! Host-attested review contracts. Attic does not launch or authenticate reviewers.

use crate::{Result, SpecError, sha256};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashSet, path::Path};

pub const UPSTREAM_REVISION: &str = "dea7761de06ce5b51c177ebb9bba0469f24af5a4";

pub const REQUIRED_REVIEWERS: &[(&str, &str)] = &[
    ("initial_zoom_out", "zoom_out"),
    ("initial_zoom_out", "inventive_catalyst"),
    ("comprehension", "comprehension"),
    ("critique", "design"),
    ("critique", "structural"),
    ("critique", "style"),
    ("intermediate_zoom_out", "zoom_out"),
    ("implementation_readiness", "implementation_readiness"),
    ("implementation_readiness", "codebase_details"),
    ("implementation_readiness", "style"),
];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Severity,
    pub description: String,
    pub disposition: Disposition,
    pub rationale: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Nit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    Open,
    Fixed,
    Contested,
    AcceptedRisk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewerReport {
    pub stage: String,
    pub persona: String,
    pub completed: bool,
    pub context_id: Option<String>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoundReport {
    pub protocol_version: u32,
    pub host: String,
    pub upstream_revision: String,
    pub document_sha256: String,
    pub verification_run_id: String,
    pub evidence_sha256: String,
    pub attempt_id: String,
    pub round: u32,
    pub reviewers: Vec<ReviewerReport>,
}

/// Return document-only line ranges, including the implementation heading.
pub fn document_ranges(bytes: &[u8]) -> Result<(usize, usize, usize)> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| SpecError::new("ERR_REVIEW_SPLIT", "document is not UTF-8"))?;
    let lines: Vec<_> = text.lines().collect();
    let mut boundaries = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut comment = false;
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start_matches(' ');
        let indentation = line.len() - trimmed.len();
        if fence.is_none() && (comment || trimmed.starts_with("<!--")) {
            comment = !trimmed.contains("-->");
            continue;
        }
        if indentation <= 3
            && let Some(marker @ ('`' | '~')) = trimmed.chars().next()
        {
            let count = trimmed.chars().take_while(|c| *c == marker).count();
            if count >= 3 {
                if let Some((open, length)) = fence {
                    if open == marker && count >= length && trimmed[count..].trim().is_empty() {
                        fence = None;
                    }
                } else {
                    fence = Some((marker, count));
                }
                continue;
            }
        }
        if fence.is_some() || indentation > 3 {
            continue;
        }
        let Some(heading) = trimmed.strip_prefix("## ") else {
            continue;
        };
        if [
            "detailed implementation plan",
            "implementation plan",
            "work breakdown",
            "implementation steps",
        ]
        .contains(&heading.trim().to_ascii_lowercase().as_str())
        {
            boundaries.push(index + 1);
        }
    }
    match boundaries.as_slice() {
        [line] if *line > 1 => Ok((1, *line, lines.len())),
        _ => Err(SpecError::new(
            "ERR_REVIEW_SPLIT",
            "expected one implementation-plan heading after the feature section",
        )),
    }
}

pub fn evidence_hash(run: &crate::verifier::Run) -> Result<String> {
    let mut edges = run.dependency_edges.clone();
    edges.sort();
    Ok(sha256(&serde_json::to_vec(&(
        &run.citation_fingerprint,
        &run.revision_set_hash,
        &run.source_receipts,
        &run.citations,
        &edges,
    ))?))
}

/// Validate coverage and return whether the round is clean.
pub fn validate(report: &RoundReport) -> Result<bool> {
    let invalid = |message| SpecError::new("ERR_REVIEW_REPORT", message);
    if report.protocol_version != 1
        || report.upstream_revision != UPSTREAM_REVISION
        || report.host.trim().is_empty()
        || report.host.len() > 200
        || report.round == 0
        || report.reviewers.len() != REQUIRED_REVIEWERS.len()
    {
        return Err(invalid("invalid review protocol, host, round, or coverage"));
    }
    let mut seen = HashSet::new();
    let mut contexts = HashSet::new();
    let mut clean = true;
    for reviewer in &report.reviewers {
        let key = (reviewer.stage.as_str(), reviewer.persona.as_str());
        if !REQUIRED_REVIEWERS.contains(&key) || !seen.insert(key) {
            return Err(invalid("unknown or duplicate stage/persona"));
        }
        if reviewer.findings.len() > 100 {
            return Err(invalid("too many findings"));
        }
        if reviewer.completed {
            let context = reviewer
                .context_id
                .as_deref()
                .filter(|s| !s.trim().is_empty() && s.len() <= 200)
                .ok_or_else(|| invalid("completed reviewer requires a fresh context id"))?;
            if !contexts.insert(context) {
                return Err(invalid("reviewer context ids must be distinct"));
            }
        } else {
            if reviewer.context_id.is_some() || !reviewer.findings.is_empty() {
                return Err(invalid(
                    "a reviewer not run cannot have a context or findings",
                ));
            }
            clean = false;
        }
        for finding in &reviewer.findings {
            if finding.description.trim().is_empty() || finding.description.len() > 4096 {
                return Err(invalid("finding description missing or too long"));
            }
            if finding.rationale.as_ref().is_some_and(|s| s.len() > 4096) {
                return Err(invalid("finding rationale too long"));
            }
            let settled = match finding.disposition {
                Disposition::Contested | Disposition::AcceptedRisk => {
                    if finding
                        .rationale
                        .as_deref()
                        .is_none_or(|s| s.trim().is_empty())
                    {
                        return Err(invalid("settled finding requires a rationale"));
                    }
                    if finding.severity == Severity::Blocker
                        && finding.disposition == Disposition::AcceptedRisk
                    {
                        return Err(invalid("blockers cannot be accepted as risks"));
                    }
                    true
                }
                Disposition::Open | Disposition::Fixed => false,
            };
            if finding.severity != Severity::Nit && !settled {
                clean = false;
            }
        }
    }
    Ok(clean)
}

fn latest_attempt(conn: &Connection, spec_uid: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT attempt_id FROM spec_review_attempts WHERE spec_uid=?1 ORDER BY rowid DESC LIMIT 1",
            [spec_uid],
            |r| r.get(0),
        )
        .optional()?)
}

fn count_rounds(conn: &Connection, attempt: &str) -> Result<u32> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM spec_review_rounds WHERE attempt_id=?1",
        [attempt],
        |r| r.get(0),
    )?)
}

fn passing(
    conn: &Connection,
    doc: &crate::state::Document,
    run: &crate::verifier::Run,
) -> Result<bool> {
    let Some(attempt) = latest_attempt(conn, &doc.spec_uid)? else {
        return Ok(false);
    };
    let evidence = evidence_hash(run)?;
    let mut statement = conn.prepare(
        "SELECT clean,run_id,doc_sha256,evidence_sha256,spec_revision
         FROM spec_review_rounds WHERE attempt_id=?1 ORDER BY round DESC LIMIT 2",
    )?;
    let rows = statement
        .query_map([attempt], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows.len() == 2
        && rows.iter().all(|(clean, id, hash, digest, revision)| {
            *clean
                && id == &run.run_id
                && hash == &run.doc_sha256
                && digest == &evidence
                && *revision == doc.revision
        }))
}

fn require_fresh_sources(run: &crate::verifier::Run) -> Result<()> {
    for finding in &run.citations {
        let Some(rel) = finding.citation.path.as_deref() else {
            continue;
        };
        if finding.citation.kind == crate::citations::Kind::AllowGlob {
            continue;
        }
        let path = crate::verifier::safe_path(Path::new(&finding.repository_root), rel)?;
        let current = crate::verifier::disk_hash(&path)?;
        if current != finding.content_hash {
            return Err(SpecError::new(
                "ERR_REVIEW_STALE",
                format!("reviewed source changed: {rel}"),
            ));
        }
    }
    Ok(())
}

impl crate::service::SpecService {
    pub fn visit_review_reports(
        &self,
        doc: &crate::state::Document,
        mut visit: impl FnMut(&RoundReport) -> Result<()>,
    ) -> Result<()> {
        self.store.read(|conn| {
            if let Some(attempt) = latest_attempt(conn, &doc.spec_uid)? {
                let mut query = conn.prepare(
                    "SELECT payload FROM spec_review_rounds WHERE attempt_id=?1 ORDER BY round",
                )?;
                let rows = query.query_map([attempt], |row| row.get::<_, String>(0))?;
                for row in rows {
                    visit(&serde_json::from_str(&row?)?)?;
                }
            }
            Ok(())
        })
    }

    fn review_run(&self, doc: &crate::state::Document) -> Result<crate::verifier::Run> {
        if doc.status != crate::state::Status::Verified {
            return Err(SpecError::new(
                "ERR_REVIEW_STATE",
                "verify the spec before review",
            ));
        }
        let run = self.run(doc.latest_passing_run_id.as_deref().ok_or_else(|| {
            SpecError::new("ERR_REVIEW_STATE", "a passing verification run is required")
        })?)?;
        if !run.passed
            || run.kind != "PRE_CODE"
            || run.spec_revision != doc.revision
            || doc.latest_run_id.as_deref() != Some(&run.run_id)
            || crate::sha256(&Self::doc_bytes(doc)?) != run.doc_sha256
        {
            return Err(SpecError::new(
                "ERR_REVIEW_STALE",
                "verification binding changed",
            ));
        }
        document_ranges(&Self::doc_bytes(doc)?)?;
        require_fresh_sources(&run)?;
        Ok(run)
    }

    pub fn review_start(&self, expected: &crate::state::Document) -> Result<Value> {
        if self.config.packs.max_review_rounds < 2 {
            return Err(SpecError::new(
                "ERR_REVIEW_CONFIG",
                "planning.packs.max_review_rounds must be at least 2 for review to converge",
            ));
        }
        let doc = self.reconciled(expected)?;
        let run = self.review_run(&doc)?;
        let attempt = uuid::Uuid::new_v4().to_string();
        let mut next = doc.clone();
        self.store.write(|tx| {
            require_fresh_sources(&run)?;
            if sha256(&Self::doc_bytes(&doc)?) != run.doc_sha256 {
                return Err(SpecError::new(
                    "ERR_REVIEW_STALE",
                    "document changed during review start",
                ));
            }
            if let Some(previous) = latest_attempt(tx, &doc.spec_uid)?
                && count_rounds(tx, &previous)? < self.config.packs.max_review_rounds
            {
                return Err(SpecError::new(
                    "ERR_REVIEW_IN_PROGRESS",
                    "resume the existing review attempt",
                ));
            }
            tx.execute(
                "INSERT INTO spec_review_attempts(attempt_id,spec_uid,created_at) VALUES(?1,?2,?3)",
                params![attempt, doc.spec_uid, self.clock.now()?],
            )?;
            crate::store::save(
                tx,
                &doc,
                &mut next,
                "review_start",
                "MCP",
                &json!({"attempt_id":attempt}),
                self.clock.now()?,
            )
        })?;
        self.review_prepare(&next)
    }

    pub fn review_prepare(&self, doc: &crate::state::Document) -> Result<Value> {
        let doc = self.effective(doc)?;
        let run = self.review_run(&doc)?;
        let bytes = Self::doc_bytes(&doc)?;
        let (first, split, end) = document_ranges(&bytes)?;
        let (attempt, count, passed, last_report) = self.store.read(|conn| {
            let attempt = latest_attempt(conn, &doc.spec_uid)?;
            let count = attempt.as_deref().map(|a| count_rounds(conn, a)).transpose()?.unwrap_or(0);
            let last_report = if let Some(attempt) = &attempt {
                let payload: Option<String> = conn.query_row(
                    "SELECT payload FROM spec_review_rounds WHERE attempt_id=?1 ORDER BY round DESC LIMIT 1",
                    [attempt], |row| row.get(0),
                ).optional()?;
                payload.map(|s| serde_json::from_str::<RoundReport>(&s)).transpose()?
            } else {
                None
            };
            Ok((attempt, count, passing(conn, &doc, &run)?, last_report))
        })?;
        Ok(json!({
            "protocol_version": 1,
            "status": if passed {"review_passed_host_attested"} else if count >= self.config.packs.max_review_rounds {"review_exhausted"} else {"review_pending"},
            "attempt_id": attempt,
            "next_round": count + 1,
            "max_review_rounds": self.config.packs.max_review_rounds,
            "last_report": last_report,
            "document_sha256": run.doc_sha256,
            "verification_run_id": run.run_id,
            "evidence_sha256": evidence_hash(&run)?,
            "upstream_revision": UPSTREAM_REVISION,
            "feature_lines": [first, split - 1],
            "implementation_lines": [split, end],
            "required_reviewers": REQUIRED_REVIEWERS,
            "trust": "host_attested; fresh independent reviewer contexts are not verified by Attic"
        }))
    }

    pub fn review_submit(
        &self,
        expected: &crate::state::Document,
        report: &RoundReport,
    ) -> Result<Value> {
        let doc = self.reconciled(expected)?;
        let run = self.review_run(&doc)?;
        let clean = validate(report)?;
        if report.document_sha256 != run.doc_sha256
            || report.verification_run_id != run.run_id
            || report.evidence_sha256 != evidence_hash(&run)?
        {
            return Err(SpecError::new(
                "ERR_REVIEW_STALE",
                "report binding differs from verification",
            ));
        }
        let payload = serde_json::to_string(report)?;
        if payload.len() > self.config.packs.byte_budget {
            return Err(SpecError::new(
                "ERR_REVIEW_REPORT",
                "report exceeds configured byte budget",
            ));
        }
        let safe = attic_discovery::secrets::preprocess(&payload, "review.json");
        if safe.content.as_deref() != Some(payload.as_str()) {
            return Err(SpecError::new(
                "ERR_REVIEW_REPORT",
                "report contains excluded or sensitive content",
            ));
        }
        let mut next = doc.clone();
        self.store.write(|tx| {
            require_fresh_sources(&run)?;
            if crate::sha256(&Self::doc_bytes(&doc)?) != run.doc_sha256 {
                return Err(SpecError::new("ERR_REVIEW_STALE", "document changed during submission"));
            }
            let current = latest_attempt(tx, &doc.spec_uid)?
                .ok_or_else(|| SpecError::new("ERR_REVIEW_STATE", "start a review attempt first"))?;
            if current != report.attempt_id {
                return Err(SpecError::new("ERR_REVIEW_STALE", "review attempt changed"));
            }
            let count = count_rounds(tx, &current)?;
            if count >= self.config.packs.max_review_rounds || report.round != count + 1 {
                return Err(SpecError::new("ERR_REVIEW_ROUND", "round out of order or limit reached"));
            }
            let mut query = tx.prepare("SELECT payload FROM spec_review_rounds WHERE attempt_id=?1")?;
            let prior = query.query_map([&current], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let ids: HashSet<_> = report.reviewers.iter().filter_map(|r| r.context_id.as_deref()).collect();
            for old in prior {
                let old: RoundReport = serde_json::from_str(&old)?;
                if old.reviewers.iter().filter_map(|r| r.context_id.as_deref()).any(|id| ids.contains(id)) {
                    return Err(SpecError::new("ERR_REVIEW_REPORT", "reviewer context reused across rounds"));
                }
            }
            tx.execute(
                "INSERT INTO spec_review_rounds(attempt_id,round,spec_uid,spec_revision,run_id,doc_sha256,evidence_sha256,clean,payload,created_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![current, report.round, doc.spec_uid, doc.revision, run.run_id, run.doc_sha256,
                    report.evidence_sha256, clean, payload, self.clock.now()?],
            )?;
            crate::store::save(
                tx, &doc, &mut next, "review_submit", "MCP",
                &json!({"attempt_id":current,"round":report.round,"clean":clean}), self.clock.now()?,
            )
        })?;
        self.review_prepare(&next)
    }

    pub(crate) fn require_review_binding(
        &self,
        conn: &Connection,
        doc: &crate::state::Document,
        run: &crate::verifier::Run,
    ) -> Result<()> {
        if !passing(conn, doc, run)? {
            return Err(SpecError::new(
                "ERR_REVIEW_REQUIRED",
                "two clean host-attested review rounds are required",
            ));
        }
        Ok(())
    }

    pub(crate) fn require_review(
        &self,
        conn: &Connection,
        doc: &crate::state::Document,
        run: &crate::verifier::Run,
    ) -> Result<()> {
        if sha256(&Self::doc_bytes(doc)?) != run.doc_sha256 {
            return Err(SpecError::new(
                "ERR_REVIEW_STALE",
                "reviewed document changed",
            ));
        }
        require_fresh_sources(run)?;
        self.require_review_binding(conn, doc, run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> RoundReport {
        RoundReport {
            protocol_version: 1,
            host: "test".into(),
            upstream_revision: UPSTREAM_REVISION.into(),
            document_sha256: "doc".into(),
            verification_run_id: "run".into(),
            evidence_sha256: "evidence".into(),
            attempt_id: "attempt".into(),
            round: 1,
            reviewers: REQUIRED_REVIEWERS
                .iter()
                .enumerate()
                .map(|(index, (stage, persona))| ReviewerReport {
                    stage: (*stage).into(),
                    persona: (*persona).into(),
                    completed: true,
                    context_id: Some(index.to_string()),
                    findings: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn ranges_reject_missing_duplicate_and_fenced_headings() {
        assert_eq!(
            document_ranges(b"# Spec\n## Implementation Plan\nstep\n").unwrap(),
            (1, 2, 3)
        );
        assert!(document_ranges(b"# Spec\n```\n## Implementation Plan\n```\n").is_err());
        assert!(document_ranges(b"# Spec\n````\n```\n## Implementation Plan\n````\n").is_err());
        assert!(document_ranges(b"# Spec\n    ## Implementation Plan\n").is_err());
        assert!(document_ranges(b"# Spec\n<!--\n## Implementation Plan\n-->\n").is_err());
        assert!(document_ranges(b"# Spec\n## Implementation Plan\n## Work Breakdown\n").is_err());
    }

    #[test]
    fn incomplete_duplicate_and_unsettled_reviews_cannot_pass() {
        let mut r = report();
        assert!(validate(&r).unwrap());
        let first = r.reviewers[0].context_id.take();
        r.reviewers[0].completed = false;
        assert!(!validate(&r).unwrap());
        r.reviewers[0].completed = true;
        r.reviewers[0].context_id = first;
        r.reviewers[0].findings.push(Finding {
            severity: Severity::Major,
            description: "missing edge case".into(),
            disposition: Disposition::Fixed,
            rationale: None,
        });
        assert!(!validate(&r).unwrap());
        r.reviewers[0].findings[0].disposition = Disposition::Contested;
        r.reviewers[0].findings[0].rationale = Some("not applicable".into());
        assert!(validate(&r).unwrap());
        r.reviewers[1].context_id = r.reviewers[0].context_id.clone();
        assert!(validate(&r).is_err());
    }
}
