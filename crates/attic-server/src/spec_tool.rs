use crate::AtticServer;

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn pack_admission_is_shared_across_routed_clones_and_released() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("egm-tests");
        std::fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        std::fs::write(
            dir.path().join("attic.toml"),
            "[planning]\nenabled=true\n[planning.packs]\nmax_concurrent=1\n",
        )
        .unwrap();
        let server = AtticServer::new(&dir.path().join("catalog.db")).unwrap();
        let permit = server
            .spec
            .as_ref()
            .unwrap()
            .packs
            .clone()
            .try_acquire_owned()
            .unwrap();
        let request = HashMap::from([
            ("action".into(), json!("pack")),
            ("spec_id".into(), json!("one")),
        ]);
        let result = server
            .clone()
            .handle_spec(request.clone(), HashSet::new(), true)
            .await;
        assert!(serde_json::to_string(&result).unwrap().contains("ERR_BUSY"));
        drop(permit);
        let result = server.handle_spec(request, HashSet::new(), true).await;
        assert!(
            serde_json::to_string(&result)
                .unwrap()
                .contains("ERR_SPEC_NOT_FOUND")
        );
        assert_eq!(server.spec.as_ref().unwrap().packs.available_permits(), 1);
    }

    #[test]
    fn unavailable_unrelated_root_does_not_block_status_heartbeat_or_deleted_doc_abandon() {
        use attic_spec::{
            service::SpecService,
            state::{Document, Status},
        };
        for only_a_enabled in [true, false] {
            let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
                .join("egm-tests");
            std::fs::create_dir_all(&fixtures).unwrap();
            let dir = tempfile::tempdir_in(fixtures).unwrap();
            let a = dir.path().join("a");
            let b = dir.path().join("b");
            for root in [&a, &b] {
                std::fs::create_dir_all(root).unwrap();
                std::fs::write(root.join("lib.rs"), "pub fn fixture() {}\n").unwrap();
            }
            let a = a.canonicalize().unwrap();
            let b = b.canonicalize().unwrap();
            let db = dir.path().join("catalog.db");
            let active = {
                let (conn, pool) = attic_storage::open_db(&db).unwrap();
                attic_storage::run_migrations(&conn).unwrap();
                let queue = attic_storage::WriterQueue::new(conn).unwrap();
                for root in [&a, &b] {
                    attic_indexing::index_repository(
                        &attic_indexing::IndexingStore {
                            readers: &pool,
                            writer: &queue.handle(),
                        },
                        root,
                        &attic_discovery::DiscoveryPolicy::default_git(),
                        &attic_indexing::IndexOptions::default(),
                    )
                    .unwrap();
                }
                pool.with_reader(|conn| {
                    let mut query = conn.prepare("SELECT id FROM core_repositories")?;
                    Ok(query
                        .query_map([], |r| r.get::<_, String>(0))?
                        .collect::<rusqlite::Result<HashSet<_>>>()?)
                })
                .unwrap()
            };
            assert_eq!(active.len(), 2);
            let roots = if only_a_enabled {
                format!(
                    "roots=[{}]\n",
                    serde_json::to_string(&a.to_string_lossy()).unwrap()
                )
            } else {
                String::new()
            };
            std::fs::write(
                dir.path().join("attic.toml"),
                format!("[planning]\nenabled=true\n{roots}"),
            )
            .unwrap();
            let server = AtticServer::new(&db).unwrap();
            let service = server.spec.as_ref().unwrap();
            let path = a.join("docs").join("designs").join("one.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "# fixture\n").unwrap();
            let now = attic_spec::now_micros().unwrap();
            let mut doc = Document::new(
                "one",
                &verifier::identity(&a),
                "docs/designs/one.md",
                "one",
                now,
            );
            doc.status = Status::Implementing;
            doc.approved_doc_sha256 =
                Some(attic_spec::sha256(&SpecService::doc_bytes(&doc).unwrap()));
            doc.lease_id = Some("fixture-lease".into());
            doc.lease_expires_at = Some(now + 600_000_000);
            doc.fencing_token = 1;
            service
                .store
                .write(|tx| attic_spec::store::insert(tx, &doc))
                .unwrap();
            std::fs::rename(&b, b.with_file_name("b-unavailable")).unwrap();
            let dispatch =
                |value| server.spec_dispatch(serde_json::from_value(value).unwrap(), &active);
            assert_eq!(
                dispatch(json!({"action":"status"}))
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                dispatch(json!({"action":"status","spec_id":"one","root":a})).unwrap()["status"],
                "IMPLEMENTING"
            );
            assert_eq!(
                dispatch(json!({"action":"heartbeat","spec_id":"one","root":a,
                "lease_id":"fixture-lease","fencing_token":1}))
                .unwrap()["status"],
                "IMPLEMENTING"
            );
            let packs = service.data_dir.join("packs").join(&doc.spec_uid);
            std::fs::create_dir_all(&packs).unwrap();
            std::fs::write(packs.join("old.md"), "fixture").unwrap();
            std::fs::remove_file(path).unwrap();
            let abandoned = dispatch(
                json!({"action":"abandon","spec_id":"one","root":a,"reason":"deleted document"}),
            )
            .unwrap();
            assert_eq!(abandoned["status"], "ABANDONED");
            assert!(!packs.exists());
            let persisted = service.resolve("one", None).unwrap();
            assert!(persisted.lease_id.is_none());
            assert!(persisted.fencing_token > doc.fencing_token);
        }
    }
}
use attic_spec::{
    Result, SpecError,
    verifier::{self, Repository},
};
use rmcp::model::{CallToolResult, ContentBlock, Tool};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

struct RepositoryScope {
    id: String,
    root: std::path::PathBuf,
    name: String,
}

fn resolve_scope<'a>(repos: &'a [RepositoryScope], alias: &str) -> Result<&'a RepositoryScope> {
    let basename: Vec<_> = repos
        .iter()
        .filter(|r| {
            r.root
                .file_name()
                .is_some_and(|s| s.to_string_lossy() == alias)
        })
        .collect();
    let found = if basename.is_empty() {
        repos
            .iter()
            .filter(|r| r.name == alias || r.id == alias || verifier::identity(&r.root) == alias)
            .collect()
    } else {
        basename
    };
    match found.as_slice() {
        [repo] => Ok(repo),
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub action: String,
    pub spec_id: Option<String>,
    pub root: Option<String>,
    pub persona: Option<String>,
    pub delivery: Option<String>,
    pub lease_id: Option<String>,
    pub fencing_token: Option<i64>,
    pub wave_number: Option<u32>,
    pub repository: Option<String>,
    pub reason: Option<String>,
    pub review_report: Option<attic_spec::review::RoundReport>,
}

pub(crate) fn tool() -> Tool {
    let finding = json!({"type":"object","additionalProperties":false,
    "required":["severity","description","disposition","rationale"],
    "properties":{
        "severity":{"type":"string","enum":["blocker","major","nit"]},
        "description":{"type":"string","maxLength":4096},
        "disposition":{"type":"string","enum":["open","fixed","contested","accepted_risk"]},
        "rationale":{"type":["string","null"],"maxLength":4096}
    }});
    let reviewer = json!({"type":"object","additionalProperties":false,
    "required":["stage","persona","completed","context_id","findings"],
    "properties":{
        "stage":{"type":"string"},"persona":{"type":"string"},
        "completed":{"type":"boolean"},"context_id":{"type":["string","null"]},
        "findings":{"type":"array","maxItems":100,"items":finding}
    }});
    let report = json!({"type":"object","additionalProperties":false,
    "required":["protocol_version","host","upstream_revision","document_sha256","verification_run_id","evidence_sha256","attempt_id","round","reviewers"],
    "properties":{
        "protocol_version":{"type":"integer","const":1},
        "host":{"type":"string","minLength":1,"maxLength":200},
        "upstream_revision":{"type":"string"},"document_sha256":{"type":"string"},
        "verification_run_id":{"type":"string"},"evidence_sha256":{"type":"string"},
        "attempt_id":{"type":"string"},"round":{"type":"integer","minimum":1},
        "reviewers":{"type":"array","minItems":10,"maxItems":10,"items":reviewer}
    }});
    Tool::new("spec","Spec-first EGM workflow. Human-only approval is available ONLY via attic spec approve; never run approval yourself.",
        std::sync::Arc::new(json!({"type":"object","additionalProperties":false,"required":["action"],"properties":{
            "action":{"type":"string","enum":["register","status","verify","pack","review_start","review_prepare","review_submit","begin","heartbeat","wave_complete","resume","drift","reopen","abandon"]},
            "spec_id":{"type":"string","pattern":"^[a-z0-9][a-z0-9_-]{0,62}$"},"root":{"type":"string"},
            "persona":{"type":"string","enum":["zoom_out","codebase_details","implementation_readiness"]},
            "delivery":{"type":"string","enum":["file","inline"],"default":"file"},
            "lease_id":{"type":"string"},"fencing_token":{"type":"integer","minimum":0},
            "wave_number":{"type":"integer","minimum":1},"repository":{"type":"string"},
            "reason":{"type":"string","maxLength":500},"review_report":report
        }}).as_object().cloned().unwrap_or_default()))
}

fn required<'a>(value: Option<&'a str>, name: &str) -> Result<&'a str> {
    value
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| SpecError::new("ERR_INVALID_ARGUMENT", format!("{name} is required")))
}

impl AtticServer {
    pub(crate) async fn handle_spec(
        &self,
        args: HashMap<String, Value>,
        active: HashSet<String>,
        configured: bool,
    ) -> CallToolResult {
        let request = serde_json::from_value::<Request>(json!(args))
            .map_err(|e| SpecError::new("ERR_INVALID_ARGUMENT", e.to_string()));
        let outcome = match request {
            Err(error) => Err(error),
            Ok(_) if self.spec.is_none() => Err(SpecError::new(
                "ERR_PLANNING_DISABLED",
                "planning is disabled",
            )),
            Ok(_) if !configured => Err(SpecError::new(
                "ERR_ROOT_NOT_ACTIVE",
                "workspace not configured",
            )),
            Ok(request) => {
                let permit = if request.action == "pack" {
                    self.spec
                        .as_ref()
                        .map(|s| {
                            s.packs
                                .clone()
                                .try_acquire_owned()
                                .map_err(|_| SpecError::busy())
                        })
                        .transpose()
                } else {
                    Ok(None)
                };
                match permit {
                    Err(error) => Err(error),
                    Ok(permit) => {
                        let server = self.clone();
                        match tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            server.spec_dispatch(request, &active)
                        })
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(SpecError::new("ERR_SPEC_TASK", "spec task failed")),
                        }
                    }
                }
            }
        };
        match outcome {
            Ok(data) => CallToolResult::success(vec![ContentBlock::text(
                json!({"ok":true,"spec_id":data.get("spec_id"),"status":data.get("status"),
                    "revision":data.get("revision"),"next_required_action":data.get("next_required_action"),"data":data}).to_string(),
            )]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(
                json!({"ok":false,"error":error}).to_string(),
            )]),
        }
    }
    fn spec_dispatch(&self, request: Request, active: &HashSet<String>) -> Result<Value> {
        let service = self
            .spec
            .as_ref()
            .ok_or_else(|| SpecError::new("ERR_PLANNING_DISABLED", "planning is disabled"))?;
        let enabled: HashSet<_> = service
            .config
            .roots
            .iter()
            .map(|r| {
                let path = std::path::Path::new(r);
                verifier::identity(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
            })
            .collect();
        let mut scopes = Vec::new();
        for id in active {
            let (root, name) = self
                .pool
                .with_reader(|conn| {
                    conn.query_row(
                        "SELECT root_path,display_name FROM core_repositories WHERE id=?1",
                        [id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .map_err(Into::into)
                })
                .map_err(|e| SpecError::new("ERR_INDEX_READ", e.to_string()))?;
            let root = std::path::PathBuf::from(root);
            if !enabled.is_empty() && !enabled.contains(&verifier::identity(&root)) {
                continue;
            }
            scopes.push(RepositoryScope {
                id: id.clone(),
                root,
                name,
            });
        }
        let repository = |scope: &RepositoryScope| -> Result<Repository> {
            let root = scope.root.canonicalize()?;
            let local = self
                .repository_server(&scope.id)
                .map_err(|e| SpecError::new("ERR_INDEX_READ", e.to_string()))?;
            Ok(Repository {
                id: scope.id.clone(),
                root,
                name: scope.name.clone(),
                pool: local.pool.clone(),
                lease: local.repository_lease.clone(),
                policy: self.discovery_policy(),
            })
        };
        let root = request
            .root
            .as_deref()
            .map(|r| {
                std::path::Path::new(r)
                    .canonicalize()
                    .map(|p| verifier::identity(&p))
            })
            .transpose()?;
        if request.action == "status" && request.spec_id.is_none() {
            return Ok(json!(
                service
                    .store
                    .documents()?
                    .iter()
                    .filter(|d| scopes
                        .iter()
                        .any(|r| verifier::identity(&r.root) == d.owner_root_path)
                        && root.as_ref().is_none_or(|r| r == &d.owner_root_path))
                    .map(|d| service.status(d))
                    .collect::<Result<Vec<_>>>()?
            ));
        }
        let id = required(request.spec_id.as_deref(), "spec_id")?;
        if !attic_spec::citations::valid_id(id) {
            return Err(SpecError::new("ERR_INVALID_SPEC_ID", "invalid spec id"));
        }
        if request.action == "register" {
            let root = required(root.as_deref(), "root")?;
            let repo = scopes
                .iter()
                .find(|r| verifier::identity(&r.root) == root)
                .ok_or_else(|| {
                    SpecError::new(
                        "ERR_ROOT_NOT_ACTIVE",
                        "root not active and planning-enabled",
                    )
                })?;
            return service.status(&service.register(id, &repository(repo)?)?);
        }
        let doc = service.resolve(id, root.as_deref())?;
        if !scopes
            .iter()
            .any(|r| verifier::identity(&r.root) == doc.owner_root_path)
        {
            return Err(SpecError::new(
                "ERR_ROOT_NOT_ACTIVE",
                "spec owner root is not active",
            ));
        }
        let metadata_update = match request.action.as_str() {
            "status" => return service.status(&doc),
            "review_start" => return service.review_start(&doc),
            "review_prepare" => return service.review_prepare(&doc),
            "review_submit" => {
                return service.review_submit(
                    &doc,
                    request.review_report.as_ref().ok_or_else(|| {
                        SpecError::new("ERR_INVALID_ARGUMENT", "review_report is required")
                    })?,
                );
            }
            "heartbeat" => Some(service.heartbeat(
                &doc,
                required(request.lease_id.as_deref(), "lease_id")?,
                request.fencing_token.filter(|f| *f >= 0).ok_or_else(|| {
                    SpecError::new("ERR_INVALID_ARGUMENT", "nonnegative fencing_token required")
                })?,
            )?),
            "resume" => Some(service.resume(&doc)?),
            "wave_complete" => Some(
                service.wave_complete(
                    &doc,
                    required(request.lease_id.as_deref(), "lease_id")?,
                    request.fencing_token.filter(|f| *f >= 0).ok_or_else(|| {
                        SpecError::new("ERR_INVALID_ARGUMENT", "nonnegative fencing_token required")
                    })?,
                    request.wave_number.filter(|n| *n > 0).ok_or_else(|| {
                        SpecError::new("ERR_INVALID_ARGUMENT", "positive wave_number required")
                    })?,
                    &resolve_scope(
                        &scopes,
                        required(request.repository.as_deref(), "repository")?,
                    )?
                    .id,
                )?,
            ),
            "reopen" | "abandon" => {
                let current = service.reconciled(&doc)?;
                let reason = required(request.reason.as_deref(), "reason")?;
                let result = if request.action == "reopen" {
                    service
                        .store
                        .reopen(&current, reason, "MCP", service.clock.now()?)?
                } else {
                    service
                        .store
                        .abandon(&current, reason, "MCP", service.clock.now()?)?
                };
                service.invalidate_packs(&result)?;
                Some(result)
            }
            "verify" | "begin" | "drift" | "pack" => None,
            _ => {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    "unknown spec action; approval is not exposed via MCP",
                ));
            }
        };
        if let Some(updated) = metadata_update {
            return service.status(&updated);
        }
        let repos = scopes.iter().map(repository).collect::<Result<Vec<_>>>()?;
        let updated = match request.action.as_str() {
            "verify" | "begin" | "drift" => {
                let edges=self.pool.with_reader(|conn|{
                    let mut query=conn.prepare("SELECT source_repository_id,target_repository_id,resolution FROM core_relationships WHERE rel_type='DEPENDS_ON' AND freshness_state='CURRENT' AND resolution<>'INFERRED'")?;
                    Ok(query.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?)
                }).map_err(|e|SpecError::new("ERR_INDEX_READ",e.to_string()))?;
                service.verify(
                    &doc,
                    request.action == "drift",
                    &repos,
                    &edges,
                    request.lease_id.as_deref().zip(request.fencing_token),
                    request.action == "begin",
                )?
            }
            "pack" => {
                let persona = required(request.persona.as_deref(), "persona")?;
                let mut context = String::new();
                if persona == "zoom_out" {
                    let run = service.run(doc.latest_run_id.as_deref().ok_or_else(|| {
                        SpecError::new("ERR_PACK_REQUIRES_RUN", "verify before building packs")
                    })?)?;
                    let ids: HashSet<_> = run
                        .citations
                        .iter()
                        .flat_map(|f| {
                            std::iter::once(f.repository_id.as_str())
                                .chain(f.dependency_repository_id.as_deref())
                        })
                        .collect();
                    for repo in repos.iter().filter(|r| ids.contains(r.id.as_str())) {
                        if context.len() >= service.config.packs.byte_budget {
                            context.push_str("[TRUNCATED repo_map]");
                            break;
                        }
                        let args = HashMap::from([("repository_id".into(), json!(repo.id))]);
                        let mut retries = 0;
                        let result = loop {
                            match crate::handlers::repo_map::handle_repo_map(
                                &repo.pool,
                                &args,
                                active,
                                &HashMap::new(),
                                &HashMap::new(),
                            ) {
                                Ok(result) => break result,
                                Err(crate::ServerError::Storage(
                                    attic_storage::StorageError::PoolExhausted,
                                )) if retries < 3 => {
                                    std::thread::sleep(std::time::Duration::from_millis(
                                        50 << retries,
                                    ));
                                    retries += 1;
                                }
                                Err(crate::ServerError::Storage(
                                    attic_storage::StorageError::PoolExhausted,
                                )) => return Err(SpecError::busy()),
                                Err(error) => {
                                    return Err(SpecError::new(
                                        "ERR_PACK_READ_FAILED",
                                        error.to_string(),
                                    ));
                                }
                            }
                        };
                        context
                            .push_str(&format!("\n## Existing repo_map summary: {}\n", repo.name));
                        for block in result.content {
                            if let ContentBlock::Text(text) = block {
                                let map: Value = serde_json::from_str(&text.text).map_err(|e| {
                                    SpecError::new("ERR_PACK_READ_FAILED", e.to_string())
                                })?;
                                let incomplete =
                                    map["coverage"]["complete"].as_bool() == Some(false);
                                let limit = service
                                    .config
                                    .packs
                                    .byte_budget
                                    .saturating_sub(context.len())
                                    / 4;
                                context.extend(text.text.chars().take(limit));
                                if text.text.chars().count() > limit || incomplete {
                                    context.push_str("[TRUNCATED repo_map]");
                                }
                            }
                        }
                    }
                }
                return service.pack_with_context(
                    &doc,
                    persona,
                    request.delivery.as_deref().unwrap_or("file"),
                    &repos,
                    &context,
                );
            }
            _ => {
                return Err(SpecError::new(
                    "ERR_INVALID_ARGUMENT",
                    "unknown spec action; approval is not exposed via MCP",
                ));
            }
        };
        service.status(&updated)
    }
}
