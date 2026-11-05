use crate::*;
use std::sync::RwLock;

#[derive(Clone)]
pub(crate) struct StartupServer {
    state: Arc<RwLock<State>>,
    initialization: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

enum State {
    Initializing,
    Ready(Box<AtticServer>),
    Failed(String),
    Stopping,
}

impl StartupServer {
    pub(crate) fn start(db_path: &Path, paths: &attic_core::AtticPaths) -> Self {
        let db_path = db_path.to_path_buf();
        let paths = paths.clone();
        Self::initialize(move || build_server_and_enricher(&db_path, &paths))
    }

    fn initialize(
        initialize: impl FnOnce() -> anyhow::Result<AtticServer> + Send + 'static,
    ) -> Self {
        let state = Arc::new(RwLock::new(State::Initializing));
        let shared = state.clone();
        let initialization = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(initialize).await;
            let next = match result {
                Ok(Ok(server)) => State::Ready(Box::new(server)),
                Ok(Err(error)) => {
                    error!("index initialization failed; content unavailable: {error:#}");
                    State::Failed(format!("{error:#}"))
                }
                Err(error) => {
                    error!("index initialization task failed: {error}");
                    State::Failed(format!("initialization task failed: {error}"))
                }
            };
            *shared.write().unwrap_or_else(|e| e.into_inner()) = next;
        });
        Self {
            state,
            initialization: Arc::new(Mutex::new(Some(initialization))),
        }
    }

    pub(crate) fn refresh_resources(&self) {
        if let State::Ready(server) = &*self.state.read().unwrap_or_else(|e| e.into_inner())
            && let Some(monitor) = &server.resource_monitor
        {
            monitor.refresh_process_memory();
            let _ = monitor.guidance_pressure();
        }
    }

    fn engine(&self) -> Result<AtticServer, Value> {
        match &*self.state.read().unwrap_or_else(|e| e.into_inner()) {
            State::Ready(server) => Ok(server.as_ref().clone()),
            State::Initializing => Err(json!({
                "status": "initializing",
                "content_available": false,
                "retryable": true,
                "retry_after_ms": 1000,
                "message": "Verifying local indexes. MCP remains connected; content is not served before verification completes."
            })),
            State::Failed(reason) => Err(json!({
                "status": "failed",
                "content_available": false,
                "retryable": false,
                "error": reason,
            })),
            State::Stopping => Err(json!({
                "status": "stopping",
                "content_available": false,
                "retryable": true,
            })),
        }
    }

    pub(crate) async fn shutdown(&self, reason: &str) {
        let initialization = self
            .initialization
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(task) = initialization
            && let Err(error) = task.await
        {
            warn!("initialization join failed during shutdown: {error}");
        }
        let previous = {
            let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *state, State::Stopping)
        };
        if let State::Ready(server) = previous {
            let handles = ShutdownHandles::capture(&server);
            run_shutdown_sequence(handles, reason).await;
        }
    }
}

impl ServerHandler for StartupServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: make_tools(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        cx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match self.engine() {
            Ok(server) => server.call_tool(request, cx).await,
            Err(state) => {
                let content = vec![ContentBlock::text(state.to_string())];
                Ok(if request.name.as_ref() == "status" {
                    CallToolResult::success(content)
                } else {
                    CallToolResult::error(content)
                }
                .into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServiceExt;

    #[tokio::test]
    async fn mcp_remains_responsive_beyond_client_timeout_and_after_reconnect() {
        let (release, blocked) = std::sync::mpsc::channel();
        let server = StartupServer::initialize(move || {
            blocked.recv().expect("release verification");
            anyhow::bail!("corrupt repository")
        });
        for connection in 0..2 {
            let (client_transport, server_transport) = tokio::io::duplex(64 * 1024);
            let endpoint = server.clone();
            let serving = tokio::spawn(async move {
                let running = endpoint.serve(server_transport).await.unwrap();
                let _ = running.waiting().await;
            });
            let client = tokio::time::timeout(Duration::from_secs(2), ().serve(client_transport))
                .await
                .expect("handshake independent of verification")
                .unwrap();
            let tools = tokio::time::timeout(Duration::from_secs(2), client.list_all_tools())
                .await
                .expect("tool discovery independent of verification")
                .unwrap();
            assert!(!tools.is_empty());
            if connection == 0 {
                tokio::time::sleep(Duration::from_secs(31)).await;
            }
            for name in ["status", "context"] {
                let response = tokio::time::timeout(
                    Duration::from_secs(2),
                    client.call_tool(CallToolRequestParams::new(name)),
                )
                .await
                .expect("tool reply independent of blocked verification")
                .unwrap();
                assert_eq!(response.is_error.unwrap_or(false), name != "status");
                let text = response.content[0].as_text().unwrap();
                let state: Value = serde_json::from_str(&text.text).unwrap();
                assert_eq!(state["status"], "initializing");
                assert_eq!(state["content_available"], false);
            }
            client.cancel().await.unwrap();
            serving.await.unwrap();
        }
        release.send(()).unwrap();
        let task = server.initialization.lock().unwrap().take().unwrap();
        task.await.unwrap();
        assert_eq!(server.engine().err().unwrap()["status"], "failed");
    }

    #[tokio::test]
    async fn handshake_and_state_do_not_wait_for_verification() {
        let (release, blocked) = std::sync::mpsc::channel();
        let server = StartupServer::initialize(move || {
            blocked.recv().expect("release initialization");
            anyhow::bail!("verification rejected index")
        });
        let _info = server.get_info();
        let state = server.engine().err().expect("not verified");
        assert_eq!(state["status"], "initializing");
        assert_eq!(state["content_available"], false);
        assert_eq!(state["retryable"], true);
        // A second transport sees the same daemon-owned initialization.
        assert_eq!(server.clone().engine().err().unwrap(), state);
        release.send(()).unwrap();
        let task = server.initialization.lock().unwrap().take().unwrap();
        task.await.unwrap();
        let failed = server.engine().err().expect("failed closed");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["retryable"], false);
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("verification rejected")
        );
        server.shutdown("test").await;
        assert_eq!(server.engine().err().unwrap()["status"], "stopping");
    }
}
