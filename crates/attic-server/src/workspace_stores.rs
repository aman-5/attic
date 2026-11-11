use crate::*;
use attic_storage::RepositoryStoreLease;
use std::sync::RwLock;

const WORKSPACE_QUERY_CONCURRENCY: usize = 4;

pub(crate) struct RepositoryRuntime {
    pub(crate) lease: Arc<RepositoryStoreLease>,
    pub(crate) scheduler: attic_incremental::SchedulerHandle,
}

impl AtticServer {
    pub(crate) fn remove_managed_repository(
        &self,
        id: &str,
        root: &Path,
    ) -> Result<(), ServerError> {
        let manager = self
            .stores
            .as_ref()
            .ok_or_else(|| ServerError::Retrieval("manager unavailable".into()))?;
        self.crossrepo_degraded
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let removal = {
            let roots = self
                .active_roots
                .read()
                .map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?;
            if roots
                .iter()
                .any(|configured| root == configured || root.starts_with(configured))
            {
                return Ok(());
            }
            let removal = manager.remove_repository(id)?;
            self.stop_watcher(id);
            removal
        };
        if removal == attic_storage::RepositoryRemoval::Removed {
            return Ok(());
        }
        let server = self.clone();
        let id = id.to_owned();
        let root = root.to_path_buf();
        let cancellation = attic_core::CancellationToken::new();
        let token = cancellation.clone();
        let job_key = format!("__remove__{id}");
        let handle = tokio::spawn(async move {
            loop {
                if token.is_cancelled() {
                    return;
                }
                let outcome = {
                    let roots = match server.active_roots.read() {
                        Ok(roots) => roots,
                        Err(_) => {
                            error!("membership lock poisoned during removal");
                            return;
                        }
                    };
                    if roots
                        .iter()
                        .any(|configured| root == *configured || root.starts_with(configured))
                    {
                        return;
                    }
                    server
                        .stores
                        .as_ref()
                        .expect("managed removal")
                        .remove_repository(&id)
                };
                match outcome {
                    Ok(attic_storage::RepositoryRemoval::PendingLeases) => {}
                    Ok(_) => return,
                    Err(error) => {
                        error!(
                            repository_id = id,
                            "repository file removal failed: {error}"
                        );
                        return;
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        self.bootstrap_jobs
            .lock()
            .map_err(|_| ServerError::Retrieval("job lock poisoned".into()))?
            .push(BootstrapJob {
                root_key: job_key,
                cancellation,
                handle,
            });
        Ok(())
    }

    pub(crate) fn start_catalog_refresh(&self) {
        let server = self.clone();
        let cancellation = attic_core::CancellationToken::new();
        let token = cancellation.clone();
        let handle = tokio::spawn(async move {
            let mut observed = HashMap::new();
            while !token.is_cancelled() {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let server = server.clone();
                let previous = observed.clone();
                match tokio::task::spawn_blocking(move || {
                    server.refresh_repository_catalogs(&previous)
                })
                .await
                {
                    Ok(Ok(next)) => observed = next,
                    Ok(Err(error)) => error!("workspace catalog refresh failed: {error}"),
                    Err(error) => error!("workspace catalog refresh task failed: {error}"),
                }
            }
        });
        match self.bootstrap_jobs.lock() {
            Ok(mut jobs) => jobs.push(BootstrapJob {
                root_key: "__catalog__".into(),
                cancellation,
                handle,
            }),
            Err(error) => {
                cancellation.cancel();
                error!("cannot register catalog worker: {error}");
            }
        }
    }

    fn catalog_is_current(&self, active_ids: &HashSet<String>) -> Result<bool, ServerError> {
        let generations = self
            .catalog_generations
            .read()
            .map_err(|_| ServerError::Retrieval("catalog generation lock poisoned".into()))?;
        let runtimes = self
            .repository_runtimes
            .read()
            .map_err(|_| ServerError::Retrieval("runtime lock poisoned".into()))?;
        Ok(active_ids.iter().all(|id| {
            runtimes.get(id).is_some_and(|runtime| {
                generations.get(id).copied()
                    == Some(
                        runtime
                            .lease
                            .generation()
                            .load(std::sync::atomic::Ordering::Acquire),
                    )
            })
        }))
    }

    fn refresh_repository_catalogs(
        &self,
        observed: &HashMap<String, u64>,
    ) -> Result<HashMap<String, u64>, ServerError> {
        let ids = {
            let roots = self
                .active_roots
                .read()
                .map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?;
            let containers = self
                .container_repo_roots
                .read()
                .map_err(|_| ServerError::Retrieval("container lock poisoned".into()))?;
            expand_active_ids(&self.pool, &roots, &containers).0
        };
        let mut next = HashMap::new();
        let mut changed = ids.len() != observed.len();
        for id in &ids {
            let local = self.repository_server(id)?;
            let generation = local
                .writer
                .generation()
                .load(std::sync::atomic::Ordering::SeqCst);
            next.insert(id.clone(), generation);
            if observed.get(id) != Some(&generation) {
                changed = true;
                self.crossrepo_degraded
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.project_repository(&local, id)?;
            }
        }
        if changed {
            self.pool.with_reader(|conn| {
                attic_crossrepo::maintenance::sync_workspace(
                    conn,
                    &self.writer,
                    &attic_crossrepo::maintenance::WorkspaceSyncOptions {
                        active_repository_ids: Some(ids.into_iter().collect()),
                        ..Default::default()
                    },
                )
                .map_err(|error| StorageError::Worker(error.to_string()))
            })?;
            *self
                .catalog_generations
                .write()
                .map_err(|_| ServerError::Retrieval("catalog generation lock poisoned".into()))? =
                next.clone();
            self.crossrepo_degraded
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(next)
    }

    pub(crate) fn project_repository(
        &self,
        local: &AtticServer,
        id: &str,
    ) -> Result<(), ServerError> {
        let paths = local.pool.with_reader(|conn| {
            let repo = id
                .parse::<attic_core::RepositoryId>()
                .map_err(|error| StorageError::Worker(error.to_string()))?;
            Ok(current_files_for_repo_map(conn, &repo, None)?
                .into_iter()
                .map(|(path, _)| path)
                .filter(|path| {
                    attic_crossrepo::manifest::is_manifest_path(path) || path.ends_with(".proto")
                })
                .collect::<Vec<_>>())
        })?;
        let lease = local.repository_lease.as_ref().ok_or_else(|| {
            ServerError::Retrieval("catalog projection requires repository lease".into())
        })?;
        attic_storage::export_repository_catalog(lease, &self.writer, &paths)?;
        Ok(())
    }

    pub(crate) fn start_repository_runtime(&self, root: &Path, id: &str) -> bool {
        if self
            .repository_runtimes
            .read()
            .is_ok_and(|runtimes| runtimes.contains_key(id))
        {
            return true;
        }
        let result = (|| -> Result<(), ServerError> {
            let local = self.repository_server(id)?;
            let lease = local
                .repository_lease
                .as_ref()
                .ok_or_else(|| ServerError::Retrieval("missing repository lease".into()))?
                .clone();
            let scheduler = attic_incremental::spawn_scheduler(
                attic_incremental::SchedulerConfig {
                    workers: 1,
                    index_options: self.index_options(),
                    ..Default::default()
                },
                local.pool.clone(),
                local.writer.clone(),
                self.discovery_policy(),
                self.resource_monitor.clone(),
            )
            .map_err(|error| ServerError::Retrieval(error.to_string()))?;
            if !local.start_watcher(root, id) {
                scheduler.shutdown();
                return Err(ServerError::Retrieval(
                    "repository watcher failed to start".into(),
                ));
            }
            self.repository_runtimes
                .write()
                .map_err(|_| ServerError::Retrieval("repository runtime lock poisoned".into()))?
                .insert(id.to_owned(), RepositoryRuntime { lease, scheduler });
            Ok(())
        })();
        match result {
            Ok(()) => true,
            Err(error) => {
                error!(repository_id = id, "repository runtime failed: {error}");
                if let Ok(mut failures) = self.watcher_start_failures.write() {
                    failures.insert(id.to_owned(), error.to_string());
                }
                false
            }
        }
    }

    pub(crate) fn repository_server(
        &self,
        repository_id: &str,
    ) -> Result<AtticServer, ServerError> {
        let Some(manager) = &self.stores else {
            return Ok(self.clone());
        };
        let cached = self
            .repository_runtimes
            .read()
            .map_err(|_| ServerError::Retrieval("repository runtime lock poisoned".into()))?
            .get(repository_id)
            .map(|runtime| runtime.lease.clone());
        let lease = match cached {
            Some(lease) => lease,
            None => Arc::new(manager.acquire(repository_id)?),
        };
        let mut server = self.clone();
        server.pool = lease.readers();
        server.writer = lease.writer_handle();
        server.db_path = lease.index_path().to_path_buf();
        server.repository_lease = Some(lease);
        server.stores = None;
        Ok(server)
    }

    fn response_json(result: CallToolResult) -> Result<Value, ServerError> {
        if result.is_error == Some(true) {
            return Err(ServerError::Retrieval(format!("{:?}", result.content)));
        }
        let text = result
            .content
            .iter()
            .filter_map(|content| content.as_text().map(|text| text.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(serde_json::from_str(&text)?)
    }

    pub(crate) fn managed_tool(
        &self,
        name: &str,
        args: &HashMap<String, Value>,
        active_ids: &HashSet<String>,
        advisory: attic_storage::resource_manager::ResourceAdvisory,
    ) -> Result<CallToolResult, ServerError> {
        let manager = self.stores.as_ref().ok_or_else(|| {
            ServerError::InvalidArg("repository store manager not initialized".into())
        })?;
        {
            if !self.catalog_is_current(active_ids)? {
                self.crossrepo_degraded
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        if name == "status" {
            let statuses = manager.list_repositories()?;
            let mut repositories = Vec::new();
            for status in statuses {
                let id = status.stable_id.clone();
                if !active_ids.contains(&id)
                    && self.knowledge_repository_id().as_deref() != Some(&id)
                {
                    continue;
                }
                let mut value = json!({
                    "repository_id": status.stable_id,
                    "root": status.root_path,
                    "index_path": status.index_path,
                    "readiness": format!("{:?}", status.readiness).to_lowercase(),
                });
                let runtimes = self.repository_runtimes.read().map_err(|_| {
                    ServerError::Retrieval("repository runtime lock poisoned".into())
                })?;
                value["content_available"] = json!(runtimes.contains_key(&id));
                if let Some(service) = self
                    .incremental
                    .read()
                    .map_err(|_| ServerError::Retrieval("incremental state lock poisoned".into()))?
                    .get(&id)
                {
                    value["incremental"] = json!({
                        "reconciliation_required": service.reconciliation_required(),
                    });
                }
                repositories.push(value);
            }
            let failures = self
                .pending_index_failed
                .lock()
                .map_err(|_| ServerError::Retrieval("index failure state lock poisoned".into()))?;
            let unavailable = self
                .unavailable_roots
                .read()
                .map_err(|_| ServerError::Retrieval("unavailable roots lock poisoned".into()))?;
            let failed: Vec<_> = failures
                .iter()
                .map(|(root, error)| {
                    json!({
                        "root": root, "error": error,
                    })
                })
                .collect();
            let configured = self
                .workspace_configured
                .load(std::sync::atomic::Ordering::SeqCst);
            let pending = self
                .bootstrap_jobs
                .lock()
                .map_err(|_| ServerError::Retrieval("bootstrap job lock poisoned".into()))?
                .iter()
                .filter(|job| {
                    !job.root_key.starts_with("__catalog__")
                        && !job.root_key.starts_with("__remove__")
                        && job.root_key != "__evictor__"
                        && !job.handle.is_finished()
                })
                .map(|job| job.root_key.clone())
                .collect::<Vec<_>>();
            let current = repositories
                .iter()
                .filter(|repository| {
                    repository["readiness"] == "ready"
                        && repository["content_available"] == true
                        && repository["repository_id"]
                            .as_str()
                            .is_some_and(|id| active_ids.contains(id))
                })
                .count();
            let mut payload = json!({
                "status": if !configured { "unconfigured" } else if !failed.is_empty() || !unavailable.is_empty() { "degraded" } else if !pending.is_empty() { "indexing" } else { "ok" },
                "storage": "repository_sqlite",
                "catalog": manager.catalog_path(),
                "repositories": repositories,
                "configured_repository_count": active_ids.len(),
                "index_failures": failed,
                "pending_index_roots": pending,
                "workspace": {
                    "configured": configured,
                    "unconfigured": !configured,
                    "configured_repository_count": self.active_roots.read().map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?.len(),
                    "current_repository_count": current,
                    "disabled_repository_count": active_ids.len().saturating_sub(current),
                    "indexing_repository_count": pending.len(),
                    "unavailable_repository_count": unavailable.len() + failed.len(),
                    "degraded": !unavailable.is_empty() || !failed.is_empty(),
                    "unavailable_repositories": unavailable.iter().map(|(path, reason)| json!({
                        "path": path,
                        "reason": reason,
                    })).collect::<Vec<_>>(),
                    "repositories": repositories,
                },
                "unavailable_roots": &*unavailable,
                "knowledge": self.knowledge.read().map_err(|_| ServerError::Retrieval("knowledge lock poisoned".into()))?.to_json(),
            });
            let knowledge = payload["knowledge"].clone();
            crate::handlers::status::append_resource_status(
                &mut payload,
                self.resource_monitor.as_deref(),
                &ResourceStatus {
                    resource_mode: self.resource_mode,
                    resource_mode_source: self.resource_mode_source,
                    effective_resources: self.effective_resources,
                    knowledge,
                },
            );
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                payload.to_string(),
            )]));
        }

        if name == "file" || name == "repo_map" {
            let id = args
                .get("repository_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
            require_active_member(active_ids, id)?;
            let local = self.repository_server(id)?;
            return if name == "file" {
                handle_file(&local.pool, args, active_ids)
            } else {
                let counters = self
                    .last_discovery_counters
                    .read()
                    .map_err(|_| ServerError::Retrieval("discovery lock poisoned".into()))?;
                let diagnostics = self
                    .last_discovery_diagnostics
                    .read()
                    .map_err(|_| ServerError::Retrieval("discovery lock poisoned".into()))?;
                handle_repo_map(&local.pool, args, active_ids, &counters, &diagnostics)
            };
        }

        if name == "debug_drain_task" {
            let mut outcomes = Vec::new();
            let mut ids: Vec<_> = active_ids.iter().collect();
            ids.sort();
            for id in ids {
                let local = self.repository_server(id)?;
                outcomes.push(Self::response_json(local.handle_debug_drain_task()?)?);
            }
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                json!({
                    "repositories": outcomes,
                })
                .to_string(),
            )]));
        }

        let started = std::time::Instant::now();
        let diagnostics = crate::handlers::context::include_diagnostics(args)?;
        let scoped = args.get("repository_id").and_then(Value::as_str);
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
        validate_filter("query", query, 512)?;
        if !matches!(
            args.get("mode").and_then(Value::as_str),
            None | Some("NORMAL" | "FAST" | "DEEP" | "SEARCH" | "REFERENCES")
        ) {
            return Err(ServerError::InvalidArg(
                "mode must be FAST|NORMAL|DEEP|SEARCH|REFERENCES".into(),
            ));
        }
        if let Some(id) = scoped {
            require_active_member(active_ids, id)?;
        }
        if args.get("mode").and_then(Value::as_str) == Some("REFERENCES") {
            return crate::handlers::references::handle_managed_references(self, args, active_ids);
        }
        let mut ids: Vec<String> = match scoped {
            Some(id) => vec![id.to_string()],
            None => active_ids.iter().cloned().collect(),
        };
        let search = args.get("mode").and_then(Value::as_str) == Some("SEARCH");
        let knowledge_scope = args.get("scope").and_then(Value::as_str) == Some("knowledge");
        let explicit_path_scope = parse_path_scope(args)?.is_some();
        let include_knowledge = if search {
            knowledge_scope && scoped.is_none()
        } else {
            !explicit_path_scope
        };
        if include_knowledge
            && let Some(knowledge) = self.knowledge_repository_id()
            && !ids.contains(&knowledge)
        {
            ids.push(knowledge);
        }
        ids.sort();
        let mut outputs = Vec::new();
        let mut documents = Vec::new();
        let mut missing = Vec::new();
        let mut lexical_results = Vec::new();
        let mut search_diagnostics = Vec::new();
        for batch in ids.chunks(WORKSPACE_QUERY_CONCURRENCY) {
            let outcomes = std::thread::scope(|scope| {
                let handles = batch
                    .iter()
                    .map(|id| {
                        let server = self.clone();
                        let id = id.clone();
                        let mut local_args = args.clone();
                        local_args.insert("repository_id".into(), json!(id));
                        if search {
                            let limit = args
                                .get("max_results")
                                .and_then(Value::as_u64)
                                .unwrap_or(25);
                            if (1..=MAX_SEARCH_RESULTS as u64).contains(&limit) {
                                local_args.insert(
                                    "max_results".into(),
                                    json!((limit * 2).min(MAX_SEARCH_RESULTS as u64)),
                                );
                            }
                        }
                        scope.spawn(move || {
                            let local = match server.repository_server(&id) {
                                Ok(local) => local,
                                Err(error) => return (id, Err(error), true),
                            };
                            let local_active = HashSet::from([id.clone()]);
                            let knowledge_id = server
                                .knowledge_repository_id()
                                .filter(|knowledge| knowledge == &id);
                            let outcome = if search {
                                handle_context(
                                    &local.pool,
                                    &local.writer,
                                    true,
                                    &local_args,
                                    &local_active,
                                    advisory,
                                    knowledge_id,
                                )
                                .and_then(Self::response_json)
                                .map(|payload| (payload, None))
                            } else {
                                crate::handlers::context::prepare_context(
                                    &local.pool,
                                    &local.writer,
                                    true,
                                    &local_args,
                                    &local_active,
                                    advisory,
                                    knowledge_id,
                                )
                            };
                            (id, outcome, false)
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle.join().map_err(|_| {
                            ServerError::Retrieval(
                                "workspace repository query worker panicked".into(),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })?;

            for (id, result, failed_to_open) in outcomes {
                let (outcome, document) = match result {
                    Ok(outcome) => outcome,
                    Err(error) if failed_to_open => {
                        missing.push(json!({"repository_id": id, "error": error.to_string()}));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                if search {
                    if !outcome["search_diagnostics"].is_null() {
                        search_diagnostics.push(json!({
                            "repository_id": id,
                            "details": outcome["search_diagnostics"],
                        }));
                    }
                    if let Some(results) = outcome["results"].as_array() {
                        for (rank, item) in results.iter().enumerate() {
                            let mut item = item.clone();
                            item["workspace_rank_score"] = json!(1.0 / (61 + rank) as f64);
                            lexical_results.push(item);
                        }
                    }
                } else {
                    if let Some(document) = document {
                        documents.push(document);
                    }
                    outputs.push(json!({"repository_id": id, "answer": outcome}));
                }
            }
        }
        let fanout_ms = started.elapsed().as_secs_f64() * 1000.0;
        let catalog_started = std::time::Instant::now();
        if !search
            && !explicit_path_scope
            && !self
                .crossrepo_degraded
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            let catalog_args = args.clone();
            let (catalog, document) = crate::handlers::context::prepare_context(
                &self.pool,
                &self.writer,
                false,
                &catalog_args,
                active_ids,
                advisory,
                None,
            )?;
            if self.catalog_is_current(active_ids)? {
                if let Some(document) = document {
                    documents.push(document);
                }
                outputs.push(json!({"scope": "workspace_relationships", "answer": catalog}));
            } else {
                self.crossrepo_degraded
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let catalog_ms = catalog_started.elapsed().as_secs_f64() * 1000.0;
        let aggregation_started = std::time::Instant::now();
        if search {
            lexical_results.sort_by(|a, b| {
                b["workspace_rank_score"]
                    .as_f64()
                    .partial_cmp(&a["workspace_rank_score"].as_f64())
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        a["repository_id"]
                            .as_str()
                            .cmp(&b["repository_id"].as_str())
                    })
            });
            let limit = args
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(25) as usize;
            lexical_results =
                crate::handlers::search::collapse_search_results(lexical_results, limit);
        }
        let mut payload = crate::handlers::context::aggregate_context_answers(
            &outputs,
            &documents,
            &missing,
            ids.len(),
            diagnostics,
        )?;
        if search {
            payload["result"] = json!(if missing.is_empty() {
                "SUCCESS"
            } else {
                "PARTIAL_SUCCESS"
            });
            payload["search_diagnostics"] = if lexical_results.is_empty() {
                json!({"repositories": search_diagnostics, "unavailable": missing})
            } else {
                Value::Null
            };
        } else {
            payload["search_diagnostics"] = Value::Null;
        }
        payload["results"] = json!(lexical_results);
        payload["duplicate_coverage"] = json!(if search {
            "bounded_candidate_window"
        } else {
            "not_applicable"
        });
        payload["crossrepo_ready"] = json!(
            !self
                .crossrepo_degraded
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        payload["telemetry"] = json!({
            "fanout_ms": fanout_ms,
            "catalog_ms": catalog_ms,
            "aggregation_ms": aggregation_started.elapsed().as_secs_f64() * 1000.0,
            "server_processing_ms": started.elapsed().as_secs_f64() * 1000.0,
            "context_bytes_before_sharing": documents.iter().map(|doc| doc.text.len()).sum::<usize>(),
            "context_bytes": payload["context"].as_str().map_or(0, str::len),
        });
        if !search {
            crate::handlers::context::apply_document_budget(
                &mut payload,
                &documents,
                crate::handlers::context::response_limit(args),
            )?;
        }
        crate::handlers::context::context_response_bounded(
            payload,
            crate::handlers::context::response_limit(args),
        )
    }
}

pub(crate) type RuntimeMap = Arc<RwLock<HashMap<String, RepositoryRuntime>>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_bootstrap_routes_index_and_search_to_separate_stores() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut server = AtticServer::new(&dir.path().join("catalog.db")).unwrap();
        server.stores = Some(Arc::new(
            attic_storage::RepositoryStoreManager::open_with_catalog_writer(
                &server.db_path,
                dir.path(),
                4,
                server.writer.clone(),
            )
            .unwrap(),
        ));
        let mut roots = Vec::new();
        for (name, token) in [
            ("one", "first_unique_token"),
            ("two", "second_unique_token"),
        ] {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("lib.rs"), format!("pub fn {token}() {{}}\n")).unwrap();
            std::fs::write(root.join("shared.rs"), "pub fn shared_flow() {}\n").unwrap();
            roots.push(root);
        }
        *server.active_roots.write().unwrap() = roots.clone();
        let first = server.bootstrap_workspace(&roots[0]).unwrap();
        let second = server.bootstrap_workspace(&roots[1]).unwrap();
        assert_ne!(first, second);
        let catalog_units = server
            .pool
            .with_reader(|conn| {
                conn.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(Into::into)
            })
            .unwrap();
        assert_eq!(
            catalog_units, 0,
            "source payloads must not be duplicated into catalog"
        );
        let args = HashMap::from([
            ("query".into(), json!("first_unique_token")),
            ("mode".into(), json!("SEARCH")),
        ]);
        let answer = AtticServer::response_json(
            server
                .managed_tool(
                    "context",
                    &args,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .unwrap(),
        )
        .unwrap();
        assert!(!answer["results"].as_array().unwrap().is_empty());
        assert!(
            answer["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["repository_id"] == first)
        );
        for query in ["first_unique_token.rs", "FormCode: value", "fd: value"] {
            let search_args = HashMap::from([
                ("query".into(), json!(query)),
                ("mode".into(), json!("SEARCH")),
            ]);
            let search_answer = AtticServer::response_json(
                server
                    .managed_tool(
                        "context",
                        &search_args,
                        &HashSet::from([first.clone(), second.clone()]),
                        attic_storage::resource_manager::ResourceAdvisory::Ok,
                    )
                    .unwrap(),
            )
            .unwrap();
            assert!(
                search_answer["results"].is_array(),
                "SEARCH query {query:?} should be treated as literal text"
            );
        }
        let references_args = HashMap::from([
            ("query".into(), json!("shared_flow")),
            ("mode".into(), json!("REFERENCES")),
            ("max_files".into(), json!(1)),
        ]);
        let references = AtticServer::response_json(
            server
                .managed_tool(
                    "context",
                    &references_args,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .unwrap(),
        )
        .unwrap();
        let coverage = references["coverage"]["repositories"].as_array().unwrap();
        assert_eq!(coverage.len(), 2, "{references}");
        let considered: u64 = coverage
            .iter()
            .map(|item| item["coverage"]["files_considered"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(considered, 1, "{references}");
        assert!(references.to_string().contains("workspace_budget"));
        let mut invalid_references = references_args.clone();
        invalid_references.insert("max_files".into(), json!(0));
        assert!(
            server
                .managed_tool(
                    "context",
                    &invalid_references,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .is_err()
        );
        server.knowledge.write().unwrap().repository_id = Some(second.clone());
        let scoped_args = HashMap::from([
            ("query".into(), json!("shared_flow")),
            ("mode".into(), json!("NORMAL")),
            ("repository_id".into(), json!(first)),
            ("path".into(), json!("shared.rs")),
        ]);
        let scoped = AtticServer::response_json(
            server
                .managed_tool(
                    "context",
                    &scoped_args,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .unwrap(),
        )
        .unwrap();
        assert!(
            !scoped["evidence"].as_array().unwrap().is_empty(),
            "{scoped}"
        );
        assert!(
            scoped["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| { entry["repository_id"] == first && entry["path"] == "shared.rs" }),
            "{scoped}"
        );
        assert_eq!(scoped["coverage"]["requested"], 1);
        server.knowledge.write().unwrap().repository_id = None;
        let answer_args = HashMap::from([
            ("query".into(), json!("where is shared_flow defined")),
            ("mode".into(), json!("NORMAL")),
        ]);
        let answer = AtticServer::response_json(
            server
                .managed_tool(
                    "context",
                    &answer_args,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(answer["repository_answers"].as_array().unwrap().len(), 2);
        assert!(answer["evidence_dropped"].as_array().unwrap().is_empty());
        assert_eq!(
            answer["evidence_dropped_summary"]["details_included"],
            false
        );
        let context = answer["context"].as_str().unwrap();
        assert!(context.contains(&first));
        assert!(context.contains(&second));
        let evidence = answer["evidence"].as_array().unwrap();
        assert!(evidence.iter().any(|item| item["repository_id"] == first));
        assert!(evidence.iter().any(|item| item["repository_id"] == second));
        for repository in answer["repository_answers"].as_array().unwrap() {
            assert!(repository.get("answer").is_none());
            assert!(repository["summary"].get("context").is_none());
            assert!(repository["summary"]["plan_id"].is_string());
        }
        for claim in answer["claims"].as_array().unwrap() {
            for id in claim["evidence_ids"].as_array().unwrap() {
                assert!(evidence.iter().any(|item| &item["evidence_id"] == id));
            }
        }
        let mut diagnostic_args = answer_args.clone();
        diagnostic_args.insert("include_diagnostics".into(), json!(true));
        let diagnostic = AtticServer::response_json(
            server
                .managed_tool(
                    "context",
                    &diagnostic_args,
                    &HashSet::from([first.clone(), second.clone()]),
                    attic_storage::resource_manager::ResourceAdvisory::Ok,
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            diagnostic["evidence_dropped_summary"]["details_included"],
            true
        );
        assert_eq!(
            diagnostic["evidence_dropped_summary"]["count"]
                .as_u64()
                .unwrap() as usize,
            diagnostic["evidence_dropped"].as_array().unwrap().len(),
        );
        assert!(answer["telemetry"]["fanout_ms"].as_f64().unwrap() >= 0.0);
        assert!(answer["telemetry"]["aggregation_ms"].as_f64().unwrap() >= 0.0);
        server.active_roots.write().unwrap().remove(0);
        server
            .stores
            .as_ref()
            .unwrap()
            .remove_repository(&first)
            .unwrap();
        assert!(server.repository_server(&first).is_err());
        assert!(server.repository_server(&second).is_ok());
    }
}
