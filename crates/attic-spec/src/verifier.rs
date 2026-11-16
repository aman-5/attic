use crate::{
    Result, SpecError,
    citations::{self, Citation, Kind},
    sha256,
};
use attic_core::config::ValidationConfig;
use attic_storage::DbPool;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Repository {
    pub id: String,
    pub root: PathBuf,
    pub name: String,
    pub pool: DbPool,
    pub lease: Option<Arc<attic_storage::RepositoryStoreLease>>,
    pub policy: attic_discovery::DiscoveryPolicy,
}

pub fn identity(path: &Path) -> String {
    attic_discovery::security::root_identity_key(path)
}

pub fn safe_path(root: &Path, rel: &str) -> Result<PathBuf> {
    if !citations::valid_path(rel) {
        return Err(SpecError::new(
            "ERR_PATH_OUTSIDE_ROOT",
            "invalid relative path",
        ));
    }
    let root = root.canonicalize()?;
    let target = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut existing = target.as_path();
    let mut tail = Vec::new();
    while !existing.try_exists()? {
        tail.push(
            existing
                .file_name()
                .ok_or_else(|| SpecError::new("ERR_PATH_OUTSIDE_ROOT", "no existing ancestor"))?
                .to_owned(),
        );
        existing = existing
            .parent()
            .ok_or_else(|| SpecError::new("ERR_PATH_OUTSIDE_ROOT", "no existing ancestor"))?;
    }
    let mut resolved = existing.canonicalize()?;
    if !attic_discovery::security::path_is_within(&resolved, &root) {
        return Err(SpecError::new(
            "ERR_PATH_OUTSIDE_ROOT",
            "path resolves outside active root",
        ));
    }
    for component in tail.iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub fn resolve<'a>(repos: &'a [Repository], alias: &str) -> Result<&'a Repository> {
    let by_name: Vec<_> = repos
        .iter()
        .filter(|r| {
            r.root
                .file_name()
                .is_some_and(|s| s.to_string_lossy() == alias)
        })
        .collect();
    let candidates = if by_name.is_empty() {
        repos
            .iter()
            .filter(|r| r.name == alias || r.id == alias || identity(&r.root) == alias)
            .collect()
    } else {
        by_name
    };
    match candidates.as_slice() {
        [one] => Ok(one),
        [] => Err(SpecError::new(
            "ERR_UNKNOWN_REPO",
            "repository alias is not active",
        )),
        _ => Err(SpecError::new(
            "ERR_AMBIGUOUS_REPO",
            "repository alias matches multiple active roots",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub ordinal: usize,
    pub citation: Citation,
    pub repository_id: String,
    pub repository_root: String,
    pub dependency_repository_id: Option<String>,
    pub dependency_root: Option<String>,
    pub tier: String,
    pub signature_hash: Option<String>,
    pub content_hash: Option<String>,
    pub generation: Option<String>,
    pub source_revision: Option<String>,
    pub occurrence: Option<String>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub message: Option<String>,
    pub error: bool,
    pub warning: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub run_id: String,
    pub spec_revision: i64,
    pub kind: String,
    pub doc_sha256: String,
    pub citation_fingerprint: String,
    pub revision_set_hash: String,
    pub citations: Vec<Finding>,
    pub passed: bool,
    pub checked_at: i64,
    pub source_receipts: Vec<serde_json::Value>,
    pub dependency_edges: Vec<(String, String, String)>,
}

pub fn fingerprint(findings: &[Finding]) -> String {
    let mut lines: Vec<_> = findings
        .iter()
        .map(|f| {
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}\0{}\n",
                f.citation.kind.as_str(),
                f.repository_root,
                f.dependency_root.as_deref().unwrap_or(""),
                f.citation.path.as_deref().unwrap_or(""),
                f.citation.symbol.as_deref().unwrap_or(""),
                f.tier,
                f.signature_hash.as_deref().unwrap_or("")
            )
        })
        .collect();
    lines.sort_unstable();
    sha256(lines.concat().as_bytes())
}

pub fn disk_hash(path: &Path) -> Result<Option<String>> {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(Some(hasher.finalize().to_hex().to_string()))
}

fn creation_allowed(repo: &Repository, path: &str) -> Result<bool> {
    use attic_discovery::{classification, policy::DiscoveryPriority, secrets, security};
    if security::is_security_forbidden(path)
        || secrets::is_known_secrets_file(path)
        || classification::classify(path, &repo.policy) == DiscoveryPriority::Ignored
    {
        return Ok(false);
    }
    let explicitly_included = repo
        .policy
        .attic_include_rules
        .iter()
        .any(|rule| classification::glob_matches(&rule.pattern, path));
    if repo.policy.git_aware && !repo.policy.include_untracked && !explicitly_included {
        return Ok(false);
    }
    let mut parent = repo.root.join(path).parent().map(Path::to_path_buf);
    while let Some(p) = parent {
        if p == repo.root {
            break;
        }
        if p.join(".git").exists() {
            return Ok(false);
        }
        parent = p.parent().map(Path::to_path_buf);
    }
    if repo.policy.git_aware && !explicitly_included && crate::git::toplevel(&repo.root).is_ok() {
        let status = std::process::Command::new("git")
            .args(["check-ignore", "--no-index", "--quiet", "--", path])
            .current_dir(&repo.root)
            .status()?;
        return match status.code() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(SpecError::new(
                "ERR_CITATION_PATH_NOT_INDEXABLE",
                "Git ignore evaluation failed",
            )),
        };
    }
    Ok(true)
}

pub fn verify(
    bytes: &[u8],
    revision: i64,
    post: bool,
    repos: &[Repository],
    edges: &[(String, String, String)],
    cfg: &ValidationConfig,
) -> Result<Run> {
    let citations = citations::parse(bytes)?;
    let started = Instant::now();
    loop {
        match verify_once(&citations, post, repos, edges, cfg) {
            Ok((findings, receipts)) => {
                let revision_set_hash = sha256(serde_json::to_string(&receipts)?.as_bytes());
                return Ok(Run {
                    run_id: uuid::Uuid::new_v4().to_string(),
                    spec_revision: revision,
                    kind: if post { "POST_CODE" } else { "PRE_CODE" }.into(),
                    doc_sha256: sha256(bytes),
                    citation_fingerprint: fingerprint(&findings),
                    revision_set_hash,
                    passed: !findings.iter().any(|f| f.error),
                    citations: findings,
                    checked_at: crate::now_micros()?,
                    source_receipts: receipts,
                    dependency_edges: edges.to_vec(),
                });
            }
            Err(error)
                if error.code == "ERR_INDEX_NOT_FRESH"
                    && started.elapsed() < Duration::from_millis(cfg.freshness_wait_ms) =>
            {
                std::thread::sleep(Duration::from_millis(250).min(
                    Duration::from_millis(cfg.freshness_wait_ms).saturating_sub(started.elapsed()),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn verify_once(
    citations: &[Citation],
    post: bool,
    repos: &[Repository],
    edges: &[(String, String, String)],
    cfg: &ValidationConfig,
) -> Result<(Vec<Finding>, Vec<serde_json::Value>)> {
    let mut groups: BTreeMap<String, Vec<(usize, &Citation)>> = BTreeMap::new();
    for (i, c) in citations.iter().enumerate() {
        groups
            .entry(resolve(repos, &c.repository)?.id.clone())
            .or_default()
            .push((i, c));
    }
    let mut findings = Vec::new();
    let mut receipts = Vec::new();
    for (id, group) in groups {
        let repo = repos
            .iter()
            .find(|r| r.id == id)
            .ok_or_else(|| SpecError::new("ERR_UNKNOWN_REPO", "active repository disappeared"))?;
        let paths: BTreeSet<_> = group
            .iter()
            .filter(|(_, c)| !matches!(c.kind, Kind::AllowGlob | Kind::CrossRepoDependency))
            .map(|(_, c)| c.path.as_deref().unwrap_or(""))
            .collect();
        let mut before = BTreeMap::new();
        let eligible = attic_discovery::walk::walk(&repo.root, &repo.policy)
            .map_err(|e| SpecError::new("ERR_CITATION_PATH_NOT_INDEXABLE", e.to_string()))?;
        for path in &paths {
            let abs = safe_path(&repo.root, path)?;
            let bytes = disk_hash(&abs)?;
            if bytes.is_none() && !creation_allowed(repo, path)? {
                return Err(SpecError::new(
                    "ERR_CITATION_PATH_NOT_INDEXABLE",
                    format!("creation path excluded by discovery/security policy: {path}"),
                ));
            }
            if bytes.is_some() && !eligible.entries.iter().any(|e| e.repo_relative == *path) {
                return Err(SpecError::new(
                    "ERR_CITATION_PATH_NOT_INDEXABLE",
                    format!("citation excluded by discovery policy: {path}"),
                ));
            }
            before.insert((*path).to_string(), bytes);
        }
        let mut retries = 0;
        let repo_findings = loop {
            let result = repo.pool.with_reader(|conn| {
                Ok(read_snapshot(
                    conn, repo, &group, &before, post, repos, edges, cfg,
                ))
            });
            match result {
                Ok(result) => break result?,
                Err(attic_storage::StorageError::PoolExhausted) if retries < 3 => {
                    std::thread::sleep(Duration::from_millis(50 << retries));
                    retries += 1;
                }
                Err(attic_storage::StorageError::PoolExhausted) => return Err(SpecError::busy()),
                Err(error) => return Err(SpecError::new("ERR_INDEX_READ", error.to_string())),
            }
        };
        for path in &paths {
            let after = disk_hash(&safe_path(&repo.root, path)?)?;
            if before.get(*path) != Some(&after) {
                return Err(not_fresh(path));
            }
        }
        receipts.push(serde_json::json!({"root":identity(&repo.root),"files":repo_findings.iter().filter(|f|f.occurrence.is_some()).map(|f|
            serde_json::json!({"path":f.citation.path,"occurrence":f.occurrence,"source_revision":f.source_revision,"generation":f.generation,"before":f.content_hash,"after":f.content_hash})).collect::<Vec<_>>()}));
        findings.extend(repo_findings);
    }
    findings.sort_by_key(|f| f.ordinal);
    Ok((findings, receipts))
}

fn not_fresh(path: &str) -> SpecError {
    let mut e = SpecError::new(
        "ERR_INDEX_NOT_FRESH",
        format!("source bytes/index disagree or moved: {path}"),
    );
    e.retry_after_ms = Some(1000);
    e
}

fn name_distance(a: &str, b: &str) -> usize {
    let b: Vec<_> = b.chars().take(128).collect();
    let mut previous: Vec<_> = (0..=b.len()).collect();
    let mut row = vec![0; b.len() + 1];
    for (i, a) in a.chars().take(128).enumerate() {
        row[0] = i + 1;
        for (j, b) in b.iter().enumerate() {
            row[j + 1] = (previous[j + 1] + 1)
                .min(row[j] + 1)
                .min(previous[j] + usize::from(a != *b));
        }
        std::mem::swap(&mut previous, &mut row);
    }
    previous[b.len()]
}

#[allow(clippy::too_many_arguments)]
fn read_snapshot(
    conn: &Connection,
    repo: &Repository,
    group: &[(usize, &Citation)],
    before: &BTreeMap<String, Option<String>>,
    post: bool,
    repos: &[Repository],
    edges: &[(String, String, String)],
    cfg: &ValidationConfig,
) -> Result<Vec<Finding>> {
    let tx = conn.unchecked_transaction()?;
    let has_receipts:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='spec_index_analysis_receipts' AND type='table')",[],|r|r.get(0))?;
    let mut results = Vec::new();
    for &(ordinal, c) in group {
        let mut f = Finding {
            ordinal,
            citation: c.clone(),
            repository_id: repo.id.clone(),
            repository_root: identity(&repo.root),
            dependency_repository_id: None,
            dependency_root: None,
            tier: String::new(),
            signature_hash: None,
            content_hash: None,
            generation: None,
            source_revision: None,
            occurrence: None,
            start_line: None,
            end_line: None,
            message: None,
            error: false,
            warning: false,
        };
        if c.kind == Kind::AllowGlob {
            f.tier = "GLOB_ACCEPTED".into();
        } else if c.kind == Kind::CrossRepoDependency {
            let dep = resolve(repos, c.dependency.as_deref().unwrap_or(""))?;
            f.dependency_repository_id = Some(dep.id.clone());
            f.dependency_root = Some(identity(&dep.root));
            f.tier = if edges.iter().any(|(a, b, _)| *a == repo.id && *b == dep.id) {
                "DEPENDENCY_VERIFIED"
            } else if cfg.require_cross_repo_check {
                "MISSING_ERROR"
            } else {
                "DEPENDENCY_UNVERIFIED"
            }
            .into();
        } else {
            let path = c.path.as_deref().unwrap_or("");
            let row:Option<(String,String,Option<String>,String,String,String)>=tx.query_row(
                "SELECT fo.id,fo.content_hash,fo.index_generation_id,fo.source_revision_id,fo.existence_state,fo.freshness_state
                 FROM core_file_occurrences fo JOIN core_file_identities fi ON fi.id=fo.file_identity_id
                 WHERE fi.repository_id=?1 AND fo.path=?2 ORDER BY fo.occurrence_seq DESC LIMIT 1",
                params![repo.id,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
            let live = row.as_ref().filter(|r| r.4.eq_ignore_ascii_case("present"));
            let indexed = live.map(|r| r.1.clone());
            if before.get(path) != Some(&indexed)
                || live.is_some_and(|r| r.5 != "CURRENT" || r.2.is_none())
            {
                return Err(not_fresh(path));
            }
            if let Some((occ, hash, generation, source, _, _)) = live {
                f.occurrence = Some(occ.clone());
                f.content_hash = Some(hash.clone());
                f.generation = generation.clone();
                f.source_revision = Some(source.clone());
                let supported = if has_receipts {
                    tx.query_row("SELECT symbols_supported=1 AND structurally_complete=1 AND fallback_used=0 AND has_errors=0 AND capability_used<>'Lexical'
                        AND content_hash=?2 AND source_revision_id=?3 AND index_generation_id=?4
                        FROM spec_index_analysis_receipts WHERE file_occurrence_id=?1",
                        params![occ,hash,source,generation],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(false)
                } else {
                    false
                };
                let mut symbols = Vec::<(String, String)>::new();
                if let Some(symbol) = &c.symbol {
                    let mut query=tx.prepare("SELECT COALESCE(so.signature,''),so.source_span
                        FROM core_symbol_occurrences so JOIN core_symbol_identities si ON si.id=so.symbol_identity_id
                        WHERE so.file_occurrence_id=?1 AND so.source_revision_id=?2 AND si.repository_id=?3
                        AND so.is_definition=1 AND si.qualified_name=?4 ORDER BY so.source_span,so.id")?;
                    symbols = query
                        .query_map(params![occ, source, repo.id, symbol], |r| {
                            Ok((r.get(0)?, r.get(1)?))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                }
                f.tier=if c.kind==Kind::TargetCreateFile&&!post{"COLLISION_ERROR"}
                    else if c.symbol.is_none(){"FILE_ONLY_VERIFIED"}
                    else if !supported {f.message=Some("symbol capability/completeness is unproven for this occurrence; reindex with planning enabled".into());"UNSUPPORTED_CONSTRUCT"}
                    else if symbols.len()>1 {f.message=Some("ambiguous qualified name; cite a unique definition".into());"COLLISION_ERROR"}
                    else if c.kind==Kind::TargetCreateSymbol&&!post {
                        if symbols.is_empty(){"PRE_CREATE_ABSENT_OK"}else{"COLLISION_ERROR"}
                    } else if symbols.len()==1 {"AST_VERIFIED"} else {"MISSING_ERROR"}.into();
                if f.tier == "AST_VERIFIED" {
                    f.signature_hash = Some(sha256(symbols[0].0.as_bytes()));
                    let span = &symbols[0].1;
                    f.start_line = span.split(':').next().and_then(|s| s.parse().ok());
                    f.end_line = span
                        .split('-')
                        .nth(1)
                        .and_then(|s| s.split(':').next())
                        .and_then(|s| s.parse().ok());
                }
                if f.tier == "MISSING_ERROR" && c.symbol.is_some() {
                    let mut q=tx.prepare("SELECT DISTINCT si.qualified_name FROM core_symbol_occurrences so JOIN core_symbol_identities si ON si.id=so.symbol_identity_id WHERE so.file_occurrence_id=?1 AND so.is_definition=1 ORDER BY si.qualified_name LIMIT 4096")?;
                    let mut names = q
                        .query_map([occ], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    names.sort_by_cached_key(|name| {
                        (
                            name_distance(c.symbol.as_deref().unwrap_or(""), name),
                            name.clone(),
                        )
                    });
                    names.truncate(5);
                    f.message = Some(format!(
                        "definition not found; nearby file candidates (bounded scan): {}",
                        names.join(", ")
                    ));
                }
            } else {
                f.tier = if c.kind == Kind::TargetCreateFile && !post {
                    "PRE_CREATE_ABSENT_OK"
                } else {
                    "MISSING_ERROR"
                }
                .into();
            }
        }
        f.error = matches!(f.tier.as_str(), "MISSING_ERROR" | "COLLISION_ERROR")
            || (f.tier == "UNSUPPORTED_CONSTRUCT"
                && (!cfg.allow_file_only_fallback || c.kind == Kind::TargetCreateSymbol));
        f.warning = matches!(
            f.tier.as_str(),
            "UNSUPPORTED_CONSTRUCT" | "DEPENDENCY_UNVERIFIED"
        ) && !f.error;
        results.push(f);
    }
    tx.commit()?;
    Ok(results)
}
