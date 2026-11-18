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
        // One refresh at a time: the background poller and answer-time
        // refreshes share this lock.
        static REFRESH: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _refresh = REFRESH.lock().unwrap_or_else(|p| p.into_inner());
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
                self.project_repository(&local, id)?;
            }
        }
        if changed {
            let ids: Vec<String> = ids.into_iter().collect();
            // Relink only when what the links are built from changed
            // (dependency / .proto content, roots, membership) — ordinary
            // source edits leave the catalog version, and the links, as is.
            let (version, latest) = self.pool.with_reader(|conn| {
                let version = attic_crossrepo::catalog::catalog_version(conn, &ids)
                    .map_err(|error| StorageError::Worker(error.to_string()))?;
                Ok((version, latest_catalog_version(conn)?))
            })?;
            if latest.as_deref() != Some(version.as_str()) {
                self.crossrepo_degraded
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                self.pool.with_reader(|conn| {
                    attic_crossrepo::maintenance::sync_workspace(
                        conn,
                        &self.writer,
                        &attic_crossrepo::maintenance::WorkspaceSyncOptions {
                            active_repository_ids: Some(ids),
                            ..Default::default()
                        },
                    )
                    .map_err(|error| StorageError::Worker(error.to_string()))
                })?;
            }
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

    /// R4 at answer time: bring the cross-repository catalog and links up
    /// to date with the (just gated) repository indexes. Returns the
    /// snapshot id the links belong to.
    pub(crate) fn ensure_catalog_current(&self) -> Result<Option<String>, ServerError> {
        let observed = self
            .catalog_generations
            .read()
            .map_err(|_| ServerError::Retrieval("catalog generation lock poisoned".into()))?
            .clone();
        self.refresh_repository_catalogs(&observed)?;
        self.latest_snapshot_id()
    }

    /// Persisted unresolved / ambiguous cross-repository dependencies of
    /// the latest snapshot (bounded).
    pub(crate) fn crossrepo_diagnostics_json(&self) -> Result<Value, ServerError> {
        Ok(self.pool.with_reader(|conn| {
            let mut stmt = conn.prepare(
                "SELECT source_repository_id, declaration, kind, detail_json
                   FROM core_crossrepo_diagnostics
                  ORDER BY kind, source_repository_id, declaration LIMIT 50",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(json!({
                    "repository_id": r.get::<_, String>(0)?,
                    "declaration": r.get::<_, String>(1)?,
                    "kind": r.get::<_, String>(2)?,
                    "detail": r.get::<_, Option<String>>(3)?
                        .and_then(|d| serde_json::from_str::<Value>(&d).ok()),
                }))
            })?;
            Ok(Value::Array(rows.collect::<Result<Vec<_>, _>>()?))
        })?)
    }

    pub(crate) fn latest_snapshot_id(&self) -> Result<Option<String>, ServerError> {
        Ok(self.pool.with_reader(|conn| {
            use rusqlite::OptionalExtension;
            Ok(conn
                .query_row(
                    "SELECT id FROM core_workspace_snapshots
                      ORDER BY created_at DESC, rowid DESC LIMIT 1",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .optional()?)
        })?)
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
            if let Some(commits) = crate::commit_mode::parse_at_commit(args)? {
                return self.commit_mode_tool(name, args, active_ids, &commits, advisory);
            }
            let id = args
                .get("repository_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ServerError::InvalidArg("repository_id required".into()))?;
            require_active_member(active_ids, id)?;
            return self.gated_read(&[id.to_owned()], args, |_pass| {
                let local = self.repository_server(id)?;
                if name == "file" {
                    crate::freshness::gated_file_read(&local.pool, args, active_ids)
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
                }
            });
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
        crate::handlers::context::validate_max_bytes(args)?;
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
        if let Some(commits) = crate::commit_mode::parse_at_commit(args)? {
            return self.commit_mode_tool(name, args, active_ids, &commits, advisory);
        }
        if args.get("mode").and_then(Value::as_str) == Some("REFERENCES") {
            let ids: Vec<String> = match scoped {
                Some(id) => vec![id.to_owned()],
                None => {
                    let mut ids: Vec<String> = active_ids.iter().cloned().collect();
                    ids.sort();
                    ids
                }
            };
            return self.gated_read(&ids, args, |pass| {
                let passed: HashSet<String> = pass.passed_ids().into_iter().collect();
                crate::handlers::references::handle_managed_references(self, args, &passed)
            });
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
        // R1: every repository is verified current before any of its index
        // data is read; repositories that cannot be verified are reported
        // and not read. Answers that include workspace relationships also
        // verify every active repository, because links depend on all of
        // their dependency files (R4).
        let include_relationships = !search && !explicit_path_scope;
        let mut gate_ids = ids.clone();
        if include_relationships {
            gate_ids.extend(active_ids.iter().cloned());
            gate_ids.sort();
            gate_ids.dedup();
        }
        self.gated_read(&gate_ids, args, |pass| {
            let passed: HashSet<String> = pass.passed_ids().into_iter().collect();
            self.managed_context_read(
                args,
                active_ids,
                advisory,
                ManagedContextScope {
                    ids: ids
                        .iter()
                        .filter(|id| passed.contains(*id))
                        .cloned()
                        .collect(),
                    unavailable: pass.unavailable.clone(),
                    started,
                    diagnostics,
                    search,
                    explicit_path_scope,
                },
            )
        })
    }

    fn managed_context_read(
        &self,
        args: &HashMap<String, Value>,
        active_ids: &HashSet<String>,
        advisory: attic_storage::resource_manager::ResourceAdvisory,
        scope: ManagedContextScope,
    ) -> Result<CallToolResult, ServerError> {
        let ManagedContextScope {
            ids,
            unavailable,
            started,
            diagnostics,
            search,
            explicit_path_scope,
        } = scope;
        let mut outputs = Vec::new();
        let mut documents = Vec::new();
        let mut missing = unavailable;
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
        let mut crossrepo_ready = false;
        if !search && !explicit_path_scope {
            // R4: links are refreshed to match the gated indexes before they
            // are read, and refused if they change while being read.
            let snapshot_before = self.ensure_catalog_current()?;
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
            if self.latest_snapshot_id()? != snapshot_before {
                return Err(crate::freshness::freshness_error(
                    "VERSION_EXPIRED",
                    true,
                    "cross-repository links changed while the answer was being read; retry",
                    Value::Null,
                ));
            }
            if let Some(document) = document {
                documents.push(document);
            }
            let mut answer = catalog;
            answer["crossrepo_diagnostics"] = self.crossrepo_diagnostics_json()?;
            // R2: an unavailable repository keeps its last indexed links
            // (never deleted on an unmounted drive) but is flagged here so
            // links through it are not presented as verified.
            answer["unavailable_repositories"] = json!(missing);
            outputs.push(json!({"scope": "workspace_relationships", "answer": answer}));
            crossrepo_ready = true;
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
        // true when this answer read freshly refreshed links; for modes that
        // do not read links, whether the background catalog has caught up.
        payload["crossrepo_ready"] = json!(
            crossrepo_ready
                || !self
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

/// `catalog_version` of the most recent workspace snapshot, if any.
fn latest_catalog_version(conn: &rusqlite::Connection) -> Result<Option<String>, StorageError> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT catalog_version FROM core_workspace_snapshots
              ORDER BY created_at DESC, rowid DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?)
}

/// Repositories a managed context read covers after the freshness gate.
pub(crate) struct ManagedContextScope {
    ids: Vec<String>,
    unavailable: Vec<Value>,
    started: std::time::Instant,
    diagnostics: bool,
    search: bool,
    explicit_path_scope: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed_server(dir: &tempfile::TempDir) -> AtticServer {
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
        server
    }

    fn call(
        server: &AtticServer,
        tool: &str,
        args: &[(&str, Value)],
        active: &HashSet<String>,
    ) -> Result<Value, ServerError> {
        let args: HashMap<String, Value> = args
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        AtticServer::response_json(server.managed_tool(
            tool,
            &args,
            active,
            attic_storage::resource_manager::ResourceAdvisory::Ok,
        )?)
    }

    /// Text blocks of a tool result (plain-text tools such as `file` carry
    /// their freshness block as the last JSON block).
    fn call_blocks(
        server: &AtticServer,
        tool: &str,
        args: &[(&str, Value)],
        active: &HashSet<String>,
    ) -> Result<Vec<String>, ServerError> {
        let args: HashMap<String, Value> = args
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect();
        let result = server.managed_tool(
            tool,
            &args,
            active,
            attic_storage::resource_manager::ResourceAdvisory::Ok,
        )?;
        Ok(result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect())
    }

    #[test]
    fn every_index_reading_tool_answers_from_freshly_checked_code() {
        // S1–S5 through the managed tools: no watcher, no scheduler — the
        // gate alone brings the index to the saved files before reading.
        let dir = tempfile::TempDir::new().unwrap();
        let server = managed_server(&dir);
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn stale_branch_token() {}\n").unwrap();
        std::fs::write(root.join("gone.rs"), "pub fn removed_file_token() {}\n").unwrap();
        *server.active_roots.write().unwrap() = vec![root.clone()];
        let id = server.bootstrap_workspace(&root).unwrap();
        let active = HashSet::from([id.clone()]);

        // "Switch branch": rewrite, delete and add files, then ask at once.
        std::fs::write(root.join("lib.rs"), "pub fn fresh_branch_token() {}\n").unwrap();
        std::fs::remove_file(root.join("gone.rs")).unwrap();
        std::fs::write(root.join("new.rs"), "pub fn added_file_token() {}\n").unwrap();

        let search = |q: &str| {
            call(
                &server,
                "context",
                &[("query", json!(q)), ("mode", json!("SEARCH"))],
                &active,
            )
            .unwrap()
        };
        let fresh = search("fresh_branch_token");
        assert_eq!(fresh["results"].as_array().unwrap().len(), 1, "{fresh}");
        let f = &fresh["freshness"];
        assert_eq!(f["repositories"][0]["repository_id"], id);
        assert_eq!(f["repositories"][0]["source_label"]["kind"], "folder");
        assert_eq!(f["repositories"][0]["verification"]["files_reindexed"], 3);
        let token = f["version_token"].as_str().unwrap().to_owned();
        assert!(
            search("stale_branch_token")["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            search("removed_file_token")["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            search("added_file_token")["results"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Same answer, later call, nothing changed: token accepted.
        let file = call_blocks(
            &server,
            "file",
            &[
                ("repository_id", json!(id)),
                ("path", json!("lib.rs")),
                ("version_token", json!(token)),
            ],
            &active,
        )
        .unwrap();
        assert!(file[0].contains("fresh_branch_token"), "{file:?}");
        let file_freshness: Value = serde_json::from_str(file.last().unwrap()).unwrap();
        assert_eq!(file_freshness["freshness"]["version_token"], json!(token));
        let map = call(
            &server,
            "repo_map",
            &[("repository_id", json!(id))],
            &active,
        )
        .unwrap();
        assert!(!map["freshness"]["version_token"].is_null());
        let refs = call(
            &server,
            "context",
            &[
                ("query", json!("fresh_branch_token")),
                ("mode", json!("REFERENCES")),
            ],
            &active,
        )
        .unwrap();
        assert!(!refs["freshness"]["version_token"].is_null(), "{refs}");
        for mode in ["FAST", "NORMAL"] {
            let answer = call(
                &server,
                "context",
                &[
                    ("query", json!("where is fresh_branch_token")),
                    ("mode", json!(mode)),
                ],
                &active,
            )
            .unwrap();
            assert!(
                !answer["freshness"]["version_token"].is_null(),
                "{mode}: {answer}"
            );
        }
        let deep = call(
            &server,
            "context",
            &[
                ("query", json!("fresh_branch_token")),
                ("mode", json!("SEARCH")),
                ("deep_verify", json!(true)),
            ],
            &active,
        )
        .unwrap();
        assert_eq!(
            deep["freshness"]["repositories"][0]["verification"]["status"],
            "full_hash"
        );

        // S14: the code changed between two calls of one answer.
        std::fs::write(root.join("lib.rs"), "pub fn third_token() {}\n").unwrap();
        let err = call(
            &server,
            "file",
            &[
                ("repository_id", json!(id)),
                ("path", json!("lib.rs")),
                ("version_token", json!(token)),
            ],
            &active,
        )
        .unwrap_err()
        .to_string();
        let err: Value = serde_json::from_str(&err).expect("structured error");
        assert_eq!(err["code"], "VERSION_EXPIRED");
        assert_eq!(err["retryable"], true);

        // S21: asking about a branch that is not checked out is refused.
        let err = call(
            &server,
            "context",
            &[
                ("query", json!("third_token")),
                ("mode", json!("SEARCH")),
                ("expected_branch", json!("prod")),
            ],
            &active,
        )
        .unwrap_err()
        .to_string();
        let err: Value = serde_json::from_str(&err).expect("structured error");
        assert_eq!(err["code"], "BRANCH_MISMATCH");
        assert_eq!(err["retryable"], false);
    }

    #[test]
    fn unavailable_root_is_reported_and_never_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = managed_server(&dir);
        let mut roots = Vec::new();
        for name in ["kept", "unplugged"] {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join("lib.rs"),
                format!("pub fn {name}_shared_token() {{}}\n"),
            )
            .unwrap();
            roots.push(root);
        }
        *server.active_roots.write().unwrap() = roots.clone();
        let kept = server.bootstrap_workspace(&roots[0]).unwrap();
        let unplugged = server.bootstrap_workspace(&roots[1]).unwrap();
        std::fs::remove_dir_all(&roots[1]).unwrap();
        let active = HashSet::from([kept.clone(), unplugged.clone()]);
        let answer = call(
            &server,
            "context",
            &[("query", json!("shared_token")), ("mode", json!("SEARCH"))],
            &active,
        )
        .unwrap();
        let unavailable = answer["freshness"]["unavailable_repositories"]
            .as_array()
            .unwrap();
        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0]["repository_id"], unplugged);
        assert_eq!(unavailable[0]["code"], "SOURCE_UNAVAILABLE");
        assert!(
            answer["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["repository_id"] == kept),
            "{answer}"
        );
        // Its index was not deleted: re-attaching the folder restores it.
        std::fs::create_dir(&roots[1]).unwrap();
        std::fs::write(
            roots[1].join("lib.rs"),
            "pub fn unplugged_shared_token() {}\n",
        )
        .unwrap();
        let served = call_blocks(
            &server,
            "file",
            &[
                ("repository_id", json!(unplugged)),
                ("path", json!("lib.rs")),
            ],
            &active,
        );
        // New directory object with identical content → full-hash check
        // succeeds and the file is served.
        let served = served.unwrap();
        assert!(served[0].contains("unplugged_shared_token"), "{served:?}");
    }

    #[test]
    fn cross_repository_links_follow_dependency_changes_at_answer_time() {
        // S12/S18/S19/S20/S29: links are rebuilt from the indexed
        // dependency files before the very next answer — no poller.
        let dir = tempfile::TempDir::new().unwrap();
        let server = managed_server(&dir);
        let mk = |name: &str, files: &[(&str, &str)]| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            for (rel, body) in files {
                std::fs::write(root.join(rel), body).unwrap();
            }
            root
        };
        let lib = mk(
            "lib",
            &[
                ("go.mod", "module example.com/acme/lib\n"),
                ("lib.go", "package lib\nfunc Pay() {}\n"),
            ],
        );
        let app = mk(
            "app",
            &[
                (
                    "go.mod",
                    "module example.com/acme/app\nrequire example.com/acme/lib v1.0.0\n",
                ),
                ("main.go", "package main\nfunc main() { checkout_flow() }\n"),
            ],
        );
        *server.active_roots.write().unwrap() = vec![lib.clone(), app.clone()];
        let lib_id = server.bootstrap_workspace(&lib).unwrap();
        let app_id = server.bootstrap_workspace(&app).unwrap();
        let active = HashSet::from([lib_id.clone(), app_id.clone()]);
        let ask = || {
            call(
                &server,
                "context",
                &[("query", json!("checkout_flow")), ("mode", json!("NORMAL"))],
                &active,
            )
            .unwrap()
        };
        let edges = || {
            server
                .pool
                .with_reader(|c| {
                    Ok(
                        attic_storage::crossrepo_ops::cross_edges_touching(c, &app_id, 100)?
                            .into_iter()
                            .filter(|e| e.source_repository_id == app_id)
                            .map(|e| e.target_repository_id)
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap()
        };
        let snapshot = || server.latest_snapshot_id().unwrap();

        let answer = ask();
        assert_eq!(answer["crossrepo_ready"], true);
        assert_eq!(
            edges(),
            vec![lib_id.clone()],
            "app → lib after first answer"
        );
        let s0 = snapshot();

        // Ordinary source edit: index changes, links are NOT rebuilt.
        std::fs::write(
            app.join("main.go"),
            "package main\nfunc main() { checkout_flow2() }\n",
        )
        .unwrap();
        ask();
        assert_eq!(snapshot(), s0, "source-only edits must not relink");

        // S18: a second repository starts providing the same module.
        let fork = mk(
            "fork",
            &[
                ("go.mod", "module example.com/acme/lib\n"),
                ("lib.go", "package lib\nfunc Pay() {}\n"),
            ],
        );
        server.active_roots.write().unwrap().push(fork.clone());
        let fork_id = server.bootstrap_workspace(&fork).unwrap();
        let active3 = HashSet::from([lib_id.clone(), app_id.clone(), fork_id.clone()]);
        let answer = call(
            &server,
            "context",
            &[
                ("query", json!("checkout_flow2")),
                ("mode", json!("NORMAL")),
            ],
            &active3,
        )
        .unwrap();
        let diagnostics = server.crossrepo_diagnostics_json().unwrap();
        assert!(
            diagnostics
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["kind"] == "AMBIGUOUS" && d["repository_id"] == app_id),
            "{diagnostics} / {answer}"
        );
        assert!(edges().is_empty(), "an ambiguous provider is never guessed");
        std::fs::remove_dir_all(&fork).unwrap();
        server.active_roots.write().unwrap().pop();

        // S12/S20: the consumer drops the dependency; ask immediately.
        std::fs::write(app.join("go.mod"), "module example.com/acme/app\n").unwrap();
        ask();
        assert!(edges().is_empty(), "dropped dependency must unlink at once");
        let s1 = snapshot();

        // Re-add it, then S19: the provider is renamed away.
        std::fs::write(
            app.join("go.mod"),
            "module example.com/acme/app\nrequire example.com/acme/lib v1.0.0\n",
        )
        .unwrap();
        ask();
        assert_eq!(edges(), vec![lib_id.clone()]);
        std::fs::write(lib.join("go.mod"), "module example.com/acme/renamed\n").unwrap();
        ask();
        assert!(edges().is_empty(), "renamed provider must unlink consumers");

        // S29: a .proto change alone produces a new link snapshot.
        let before_proto = snapshot();
        std::fs::write(lib.join("api.proto"), "syntax = \"proto3\";\n").unwrap();
        ask();
        assert_ne!(snapshot(), before_proto, ".proto changes must relink");
        assert_ne!(s1, snapshot());
    }

    #[test]
    fn at_commit_answers_from_the_named_commit_without_checkout() {
        // S22: investigate the deployed commit while another branch (with
        // uncommitted edits) is checked out.
        let git = |root: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.com")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let server = managed_server(&dir);
        let mk_repo = |name: &str, gomod: &str, code: &str| {
            let root = dir.path().join(name);
            std::fs::create_dir(&root).unwrap();
            git(&root, &["init", "-q", "-b", "main"]);
            std::fs::write(root.join("go.mod"), gomod).unwrap();
            std::fs::write(root.join("pay.go"), code).unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "release"]);
            root
        };
        let app = mk_repo(
            "app",
            "module example.com/acme/app\nrequire example.com/acme/lib v1.0.0\n",
            "package main\nfunc release_pay_token() {}\n",
        );
        let lib = mk_repo(
            "lib",
            "module example.com/acme/lib\n",
            "package lib\nfunc Lib() {}\n",
        );
        git(&app, &["checkout", "-q", "-b", "dev"]);
        std::fs::write(
            app.join("pay.go"),
            "package main\nfunc dev_pay_token() {}\n",
        )
        .unwrap();
        git(&app, &["commit", "-q", "-am", "dev"]);
        std::fs::write(
            app.join("pay.go"),
            "package main\nfunc uncommitted_token() {}\n",
        )
        .unwrap();

        *server.active_roots.write().unwrap() = vec![app.clone(), lib.clone()];
        let app_id = server.bootstrap_workspace(&app).unwrap();
        let lib_id = server.bootstrap_workspace(&lib).unwrap();
        let active = HashSet::from([app_id.clone(), lib_id.clone()]);

        let search = |q: &str, at: Value| {
            call(
                &server,
                "context",
                &[
                    ("query", json!(q)),
                    ("mode", json!("SEARCH")),
                    ("repository_id", json!(app_id)),
                    ("at_commit", at),
                ],
                &active,
            )
            .unwrap()
        };
        let hit = search("release_pay_token", json!("main"));
        assert_eq!(hit["results"].as_array().unwrap().len(), 1, "{hit}");
        assert_eq!(
            hit["freshness"]["repositories"][0]["source_label"]["kind"],
            "commit"
        );
        assert!(
            search("dev_pay_token", json!("main"))["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            search("uncommitted_token", json!("main"))["results"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let file = call_blocks(
            &server,
            "file",
            &[
                ("repository_id", json!(app_id)),
                ("path", json!("pay.go")),
                ("at_commit", json!("main")),
            ],
            &active,
        )
        .unwrap();
        assert!(file[0].contains("release_pay_token"), "{file:?}");

        // The user's checkout was not touched.
        assert_eq!(git(&app, &["rev-parse", "--abbrev-ref", "HEAD"]), "dev");
        assert!(
            std::fs::read_to_string(app.join("pay.go"))
                .unwrap()
                .contains("uncommitted_token")
        );

        // Several repositories at pinned commits: links for exactly those.
        let multi = call(
            &server,
            "context",
            &[
                ("query", json!("release_pay_token")),
                ("mode", json!("NORMAL")),
                (
                    "at_commit",
                    json!({app_id.clone(): "main", lib_id.clone(): "main"}),
                ),
            ],
            &active,
        )
        .unwrap();
        let links = multi["commit_links"].as_array().unwrap();
        assert!(
            links
                .iter()
                .any(|l| l["source_repository_id"] == app_id && l["target_repository_id"] == lib_id),
            "{multi}"
        );

        // Plain folders have no commits.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("a.txt"), "x").unwrap();
        server.active_roots.write().unwrap().push(plain.clone());
        let plain_id = server.bootstrap_workspace(&plain).unwrap();
        let err = call(
            &server,
            "file",
            &[
                ("repository_id", json!(plain_id)),
                ("path", json!("a.txt")),
                ("at_commit", json!("main")),
            ],
            &HashSet::from([plain_id.clone()]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("COMMIT_UNAVAILABLE"), "{err}");
    }

    #[test]
    fn concurrent_callers_share_checks_and_never_get_an_older_one() {
        // Many simultaneous calls → at most one running + one queued check
        // per repository; every caller still sees its own changes.
        let dir = tempfile::TempDir::new().unwrap();
        let server = managed_server(&dir);
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn before_token() {}\n").unwrap();
        *server.active_roots.write().unwrap() = vec![root.clone()];
        let id = server.bootstrap_workspace(&root).unwrap();
        let active = HashSet::from([id.clone()]);
        std::fs::write(root.join("lib.rs"), "pub fn after_token() {}\n").unwrap();
        let results: Vec<Value> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    s.spawn(|| {
                        call(
                            &server,
                            "context",
                            &[("query", json!("after_token")), ("mode", json!("SEARCH"))],
                            &active,
                        )
                        .unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for r in &results {
            assert_eq!(r["results"].as_array().unwrap().len(), 1, "{r}");
        }
        let reindexed: u64 = results
            .iter()
            .map(|r| {
                r["freshness"]["repositories"][0]["verification"]["files_reindexed"]
                    .as_u64()
                    .unwrap()
            })
            .sum();
        assert!(reindexed >= 1, "the change was indexed");
    }

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
        let repository_answers: Vec<_> = answer["repository_answers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a.get("repository_id").is_some_and(|id| !id.is_null()))
            .collect();
        assert_eq!(repository_answers.len(), 2, "{answer}");
        // The answer refreshed the cross-repository catalog itself (R4).
        assert_eq!(answer["crossrepo_ready"], true);
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
