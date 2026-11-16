use crate::{
    Result, SpecError,
    git::{self, Change},
    service::{SpecService, load_waves},
    state::{self, Status},
    store::SpecStore,
    verifier,
};
use attic_core::config::{FastPathScope, PlanningConfig};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug)]
pub struct Decision {
    pub allowed: bool,
    pub message: String,
}
fn decision(ok: bool, message: String, cfg: &PlanningConfig) -> Decision {
    Decision {
        allowed: ok || !cfg.fast_path.enforce,
        message,
    }
}

fn state_docs(store: Option<&SpecStore>) -> Result<Vec<crate::state::Document>> {
    store
        .map(SpecStore::documents)
        .transpose()
        .map(|d| d.unwrap_or_default())
}

pub fn covered(
    store: Option<&SpecStore>,
    root: &Path,
    path: &str,
    commit: bool,
    cfg: &PlanningConfig,
    change: Option<&Change>,
) -> Result<bool> {
    let key = verifier::identity(root);
    for doc in state_docs(store)? {
        let bytes = match SpecService::doc_bytes(&doc) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let effective = state::reconcile(&doc, &crate::sha256(&bytes), crate::now_micros()?)?;
        let active = effective.status == Status::Implementing;
        let grace = commit
            && effective.status == Status::Implemented
            && effective.approved_doc_sha256.as_deref() == Some(&crate::sha256(&bytes))
            && effective.implemented_at.is_some_and(|at| {
                crate::now_micros().is_ok_and(|now| {
                    now.saturating_sub(at)
                        <= i64::from(cfg.fast_path.commit_grace_hours) * 3_600_000_000
                })
            });
        if !active && !grace {
            continue;
        }
        let store = store.ok_or_else(|| SpecError::new("ERR_SPEC_DB", "missing store"))?;
        let waves = store.read(|conn| load_waves(conn, &effective))?;
        if !waves.iter().any(|w| {
            w.root == key
                && matches!(w.status.as_str(), "IN_PROGRESS" | "COMPLETED")
                && crate::waves::covers(w, path)
        }) {
            continue;
        }
        if active {
            return Ok(true);
        }
        if grace {
            if let Some(change) = change
                && receipt_matches(
                    store,
                    root,
                    path,
                    change,
                    Some((&effective.spec_uid, effective.revision)),
                )?
            {
                return Ok(true);
            }
            let run = effective
                .latest_run_id
                .as_ref()
                .map(|id| {
                    store.read(|conn| {
                        let text: String = conn.query_row(
                            "SELECT payload FROM spec_verification_runs WHERE run_id=?1",
                            [id],
                            |r| r.get(0),
                        )?;
                        Ok(serde_json::from_str::<crate::verifier::Run>(&text)?)
                    })
                })
                .transpose()?;
            if let Some(run) = run
                && run.kind == "POST_CODE"
                && run.passed
                && let Some(f) = run
                    .citations
                    .iter()
                    .find(|f| f.repository_root == key && f.citation.path.as_deref() == Some(path))
            {
                let current = verifier::disk_hash(&verifier::safe_path(root, path)?)?;
                if current == f.content_hash {
                    let top = git::toplevel(root)?;
                    let relative = top_relative(&top, root, path)?;
                    let index = git::index(&top)?;
                    if git::worktree_matches_index(&top, &relative, index.get(&relative))? {
                        return Ok(true);
                    }
                }
            }
        }
    }
    if commit
        && cfg.fast_path.scope == FastPathScope::Branch
        && let (Some(store), Some(change)) = (store, change)
    {
        return receipt_matches(store, root, path, change, None);
    }
    Ok(false)
}

fn receipt_matches(
    store: &SpecStore,
    root: &Path,
    path: &str,
    change: &Change,
    scope: Option<(&str, i64)>,
) -> Result<bool> {
    let top = git::toplevel(root)?;
    let receipt_path = if change.orig_path.is_some() {
        let Some(path) = root_relative(&top, root, &change.path).or_else(|| {
            change
                .orig_path
                .as_ref()
                .and_then(|p| root_relative(&top, root, p))
        }) else {
            return Ok(false);
        };
        path
    } else {
        path.to_string()
    };
    let relative = if change.orig_path.is_some() {
        change.path.clone()
    } else {
        top_relative(&top, root, &receipt_path)?
    };
    let index = git::index(&top)?;
    let entry = index.get(&relative);
    let mode = entry.map(|e| e.mode.as_str()).unwrap_or("000000");
    let blob = entry.map(|e| e.blob.as_str()).unwrap_or("");
    let operation = operation(
        change,
        entry.is_some(),
        change
            .orig_path
            .as_ref()
            .is_some_and(|p| index.contains_key(p)),
    );
    store.read(|conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM spec_content_receipts WHERE owner_root_path=?1 AND path=?2 AND operation=?3 AND file_mode=?4 AND blob_id=?5 AND (?6 IS NULL OR (spec_uid=?6 AND spec_revision=?7)))",
                params![verifier::identity(root),receipt_path,operation,mode,blob,scope.map(|s|s.0),scope.map(|s|s.1)],|r|r.get(0))?))
}

fn operation(change: &Change, present: bool, old_present: bool) -> String {
    if let Some(old) = &change.orig_path {
        format!("{}:{old}", if old_present { "copy" } else { "rename" })
    } else if present {
        "present".into()
    } else {
        "deleted".into()
    }
}
fn top_relative(top: &Path, root: &Path, path: &str) -> Result<String> {
    let absolute = root.join(path);
    Ok(absolute
        .strip_prefix(top)
        .map_err(|_| SpecError::new("ERR_PATH_OUTSIDE_ROOT", "root is outside Git toplevel"))?
        .to_string_lossy()
        .replace('\\', "/"))
}
fn root_relative(top: &Path, root: &Path, path: &str) -> Option<String> {
    let absolute = top.join(path);
    if let Ok(path) = absolute.strip_prefix(root) {
        return Some(path.to_string_lossy().replace('\\', "/"));
    }
    if cfg!(windows) && attic_discovery::security::path_is_within(&absolute, root) {
        return Some(
            absolute
                .components()
                .skip(root.components().count())
                .collect::<PathBuf>()
                .to_string_lossy()
                .replace('\\', "/"),
        );
    }
    None
}

pub fn capture_baseline(store: &SpecStore, root: &Path, session: &str) -> Result<()> {
    if session.is_empty() || session.len() > 500 {
        return Err(SpecError::new(
            "ERR_INVALID_ARGUMENT",
            "session_id required",
        ));
    }
    let key = verifier::identity(root);
    let existing=store.read(|conn|Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM spec_session_baselines WHERE session_id=?1 AND owner_root_path=?2)",params![session,key],|r|r.get::<_,bool>(0))?))?;
    if existing {
        return Ok(());
    }
    let top = git::toplevel(root)?;
    let mut baseline = BTreeMap::new();
    for change in git::status(&top)? {
        for path in std::iter::once(&change.path).chain(change.orig_path.iter()) {
            if let Some(rel) = root_relative(&top, root, path) {
                baseline.insert(rel.clone(), content_identity(root, &rel)?);
            }
        }
    }
    store.write(|tx| {
        tx.execute(
            "INSERT OR IGNORE INTO spec_session_baselines VALUES(?1,?2,?3,?4)",
            params![
                session,
                key,
                crate::now_micros()?,
                serde_json::to_string(&baseline)?
            ],
        )?;
        Ok(())
    })
}
fn content_identity(root: &Path, path: &str) -> Result<String> {
    let path = verifier::safe_path(root, path)?;
    Ok(verifier::disk_hash(&path)?.unwrap_or_else(|| "absent".into()))
}

pub fn check_write(
    store: Option<&SpecStore>,
    root: &Path,
    target: &Path,
    session: Option<&str>,
    cfg: &PlanningConfig,
) -> Result<Decision> {
    let rel = target
        .strip_prefix(root)
        .map_err(|_| SpecError::new("ERR_PATH_OUTSIDE_ROOT", "target outside active root"))?
        .to_string_lossy()
        .replace('\\', "/");
    let resolved = verifier::safe_path(root, &rel)?;
    if rel.starts_with(&format!("{}/", cfg.designs_dir))
        && rel.ends_with(".md")
        && std::fs::metadata(&resolved)
            .map(|m| m.len() <= 1_048_576)
            .unwrap_or(true)
    {
        return Ok(decision(
            true,
            "bounded Markdown spec authoring only; this grants no code coverage".into(),
            cfg,
        ));
    }
    if covered(store, root, &rel, false, cfg, None)? {
        return Ok(decision(true, "active wave covers target".into(), cfg));
    }
    let top = git::toplevel(root)?;
    let baseline: Option<BTreeMap<String, String>> = if let (Some(store), Some(session)) =
        (store, session)
    {
        store.read(|conn|{
            let raw:Option<String>=conn.query_row("SELECT payload FROM spec_session_baselines WHERE session_id=?1 AND owner_root_path=?2",params![session,verifier::identity(root)],|r|r.get(0)).optional()?;
            raw.map(|s|serde_json::from_str(&s).map_err(Into::into)).transpose()
        })?
    } else {
        None
    };
    let mut dirty = BTreeSet::new();
    for change in git::status(&top)? {
        for path in std::iter::once(&change.path).chain(change.orig_path.iter()) {
            let Some(path) = root_relative(&top, root, path) else {
                continue;
            };
            if covered(store, root, &path, false, cfg, None)? {
                continue;
            }
            let unchanged = baseline
                .as_ref()
                .and_then(|b| b.get(&path))
                .is_some_and(|hash| {
                    content_identity(root, &path).is_ok_and(|current| current == *hash)
                });
            if !unchanged {
                dirty.insert(path);
            }
        }
    }
    dirty.insert(rel);
    let mut binary = false;
    if resolved.is_file() {
        let mut bytes = [0; 8192];
        let n = std::fs::File::open(resolved)?.read(&mut bytes)?;
        binary = bytes[..n].contains(&0);
    }
    let ok = dirty.len() <= cfg.fast_path.max_files && (!binary || cfg.fast_path.allow_binary);
    Ok(decision(
        ok,
        format!(
            "uncovered files: {} / {}; baseline {}; create {}/<id>.md then register/verify and request human approval",
            dirty.len(),
            cfg.fast_path.max_files,
            if baseline.is_some() {
                "captured; changed-again content counts"
            } else {
                "missing: conservative accounting"
            },
            cfg.designs_dir
        ),
        cfg,
    ))
}

pub fn check_commit(
    store: Option<&SpecStore>,
    roots: &[PathBuf],
    cwd: &Path,
    cfg: &PlanningConfig,
) -> Result<Decision> {
    let top = git::toplevel(cwd)?;
    let changes = git::diff(
        &top,
        if cfg.fast_path.scope == FastPathScope::Branch {
            cfg.fast_path.base_branch.as_deref()
        } else {
            None
        },
    )?;
    let mut uncovered = Vec::new();
    let mut lines = 0u64;
    let mut binary = false;
    for change in changes {
        let mut covered_all = true;
        let mut in_scope = false;
        for path in std::iter::once(&change.path).chain(change.orig_path.iter()) {
            let absolute = top.join(path);
            let root = roots
                .iter()
                .filter(|r| attic_discovery::security::path_is_within(&absolute, r))
                .max_by_key(|r| r.components().count());
            let Some(root) = root else {
                covered_all = false;
                continue;
            };
            in_scope = true;
            let Some(rel) = root_relative(&top, root, path) else {
                covered_all = false;
                continue;
            };
            if !covered(store, root, &rel, true, cfg, Some(&change))? {
                covered_all = false;
            }
        }
        if in_scope && !covered_all {
            lines = lines
                .checked_add(change.added)
                .and_then(|n| n.checked_add(change.deleted))
                .ok_or_else(|| SpecError::new("ERR_GIT_OUTPUT", "line count overflow"))?;
            binary |= change.binary;
            uncovered.push(change.path);
        }
    }
    let ok = uncovered.len() <= cfg.fast_path.max_files
        && lines <= cfg.fast_path.max_lines as u64
        && (!binary || cfg.fast_path.allow_binary);
    Ok(decision(
        ok,
        format!(
            "uncovered staged/branch files {} / {}, lines {} / {}, binary={}; paths: {}. Git hooks are bypassable; Git history does not prove hook execution.",
            uncovered.len(),
            cfg.fast_path.max_files,
            lines,
            cfg.fast_path.max_lines,
            binary,
            uncovered.join(", ")
        ),
        cfg,
    ))
}

/// Slow index inspection occurs before the lifecycle transaction; only insert
/// identities whose actual staged bytes agree with the completed source scope.
pub fn completion_receipts(
    service: &SpecService,
    doc: &crate::state::Document,
    run: &crate::verifier::Run,
) -> Result<Vec<Value>> {
    let mut receipts = Vec::new();
    let waves = service.waves(doc)?;
    let tops: BTreeSet<_> = waves
        .iter()
        .filter_map(|w| git::toplevel(Path::new(&w.root)).ok())
        .collect();
    for top in tops {
        let index = git::index(&top)?;
        let scope = |path: &str| {
            waves
                .iter()
                .filter_map(|w| root_relative(&top, Path::new(&w.root), path).map(|p| (w, p)))
                .max_by_key(|(w, _)| Path::new(&w.root).components().count())
                .filter(|(w, p)| crate::waves::covers(w, p))
        };
        for change in git::diff(&top, None)? {
            let Some((wave, rel)) = scope(&change.path) else {
                continue;
            };
            let origin = change.orig_path.as_deref().map(scope);
            if matches!(origin, Some(None)) {
                continue;
            }
            let mut owners = vec![(wave, rel)];
            if let Some(Some((old, oldrel))) = origin
                && old.root != wave.root
            {
                owners.push((old, oldrel));
            }
            let entry = index.get(&change.path);
            if !git::worktree_matches_index(&top, &change.path, entry)? {
                continue;
            }
            for (wave, rel) in owners {
                let path = verifier::safe_path(Path::new(&wave.root), &rel)?;
                if let Some(finding) = run.citations.iter().find(|f| {
                    f.repository_root == wave.root && f.citation.path.as_deref() == Some(&rel)
                }) && verifier::disk_hash(&path)? != finding.content_hash
                {
                    return Err(SpecError::new(
                        "ERR_INDEX_NOT_FRESH",
                        "source moved before content receipt capture",
                    ));
                }
                receipts.push(json!({"root":wave.root,"path":rel,"operation":operation(&change,entry.is_some(),change.orig_path.as_ref().is_some_and(|p|index.contains_key(p))),
                    "mode":entry.map(|e|e.mode.as_str()).unwrap_or("000000"),"blob":entry.map(|e|e.blob.as_str()).unwrap_or("")}));
            }
        }
        if git::index(&top)? != index {
            return Err(SpecError::new(
                "ERR_GIT",
                "index moved during completion receipt capture",
            ));
        }
    }
    Ok(receipts)
}
