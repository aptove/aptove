//! SessionService — workspace resolution and session preparation logic.
//!
//! Encapsulates the orchestration logic from `handle_session_new` and
//! `handle_session_load` that maps an incoming connection to a workspace
//! and determines the provider/model/token configuration to use.

use std::sync::Arc;

use anyhow::Result;

use crate::builder::AgentRuntime;
use crate::config::AgentConfig;
use crate::workspace::{WorkspaceManager, WorkspaceMetadata, WorkspaceStore};

// ---------------------------------------------------------------------------
// SessionContext
// ---------------------------------------------------------------------------

/// Resolved session configuration: the provider, model, and token budget
/// to use when creating a new in-memory session.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// Name of the active LLM provider (e.g. `"claude"`).
    pub provider_name: String,
    /// Model identifier for the provider (e.g. `"claude-sonnet-4-20250514"`).
    pub model: String,
    /// Maximum context tokens for this session.
    pub max_tokens: usize,
}

// ---------------------------------------------------------------------------
// SessionService
// ---------------------------------------------------------------------------

pub struct SessionService {
    workspace_store: Arc<dyn WorkspaceStore>,
}

impl SessionService {
    pub fn new(workspace_manager: &WorkspaceManager) -> Self {
        Self {
            workspace_store: workspace_manager.workspace_store().clone(),
        }
    }

    /// Resolve (or create) a workspace for an incoming session request.
    ///
    /// - If `session_id_opt` is `Some`, the workspace with that UUID is
    ///   loaded (returning `is_new = false`) or created fresh (`is_new = false`).
    /// - Otherwise, `device_id_opt` is used to look up or create a workspace
    ///   via the runtime's `WorkspaceManager`.
    ///
    /// Returns `(workspace, is_reusing_session)` where `is_reusing_session`
    /// is `true` only if the workspace already existed when a session_id was
    /// supplied.
    pub async fn resolve_workspace(
        &self,
        session_id_opt: Option<&str>,
        device_id_opt: Option<&str>,
        runtime: &AgentRuntime,
    ) -> Result<(WorkspaceMetadata, bool)> {
        if let Some(session_id) = session_id_opt {
            match self.workspace_store.load(session_id).await {
                Ok(ws) => return Ok((ws, true)),
                Err(_) => {
                    let ws = self
                        .workspace_store
                        .create(session_id, None, None)
                        .await?;
                    return Ok((ws, false));
                }
            }
        }

        let device_id = device_id_opt.unwrap_or("default");
        let ws = runtime.workspace_manager().resolve(device_id).await?;
        Ok((ws, false))
    }

    /// Determine the provider name, model, and max token budget for a session.
    ///
    /// Applies any workspace-level config overlay on top of the global
    /// `config`, then derives the provider and model for this workspace.
    pub async fn prepare_session(
        &self,
        workspace: &WorkspaceMetadata,
        config: &AgentConfig,
        runtime: &AgentRuntime,
    ) -> Result<SessionContext> {
        let ws_config = match runtime
            .workspace_manager()
            .load_workspace_config(&workspace.uuid)
            .await
        {
            Ok(Some(ws_toml)) => {
                match crate::config::merge_toml_configs(&toml::to_string(config)?, &ws_toml) {
                    Ok(merged) => merged,
                    Err(_) => config.clone(),
                }
            }
            _ => config.clone(),
        };

        let provider_name = workspace
            .provider
            .as_deref()
            .unwrap_or(ws_config.provider.as_str())
            .to_string();
        let model = ws_config.model_for_provider(&provider_name);

        let max_tokens = match runtime.active_provider() {
            Ok(p) => p.model_info().max_context_tokens,
            Err(_) => 200_000,
        };

        Ok(SessionContext {
            provider_name,
            model,
            max_tokens,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::BindingStore;
    use crate::config::AgentConfig;
    use crate::persistence::{SessionData, SessionStore};
    use crate::provider::{LlmProvider, LlmResponse, ModelInfo, StopReason};
    use crate::types::{Message, ToolDefinition};
    use crate::workspace::{WorkspaceSummary};
    use crate::{AgentBuilder, AgentRuntime};
    use chrono::Utc;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    // -- Mock stores -------------------------------------------------------

    struct MockWorkspaceStore {
        workspaces: RwLock<HashMap<String, WorkspaceMetadata>>,
    }

    impl MockWorkspaceStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                workspaces: RwLock::new(HashMap::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceStore for MockWorkspaceStore {
        async fn create(
            &self,
            uuid: &str,
            name: Option<&str>,
            provider: Option<&str>,
        ) -> Result<WorkspaceMetadata> {
            let meta = WorkspaceMetadata {
                uuid: uuid.to_string(),
                name: name.map(|s| s.to_string()),
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                provider: provider.map(|s| s.to_string()),
            };
            self.workspaces
                .write()
                .await
                .insert(uuid.to_string(), meta.clone());
            Ok(meta)
        }

        async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata> {
            self.workspaces
                .read()
                .await
                .get(uuid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("workspace not found: {}", uuid))
        }

        async fn list(&self) -> Result<Vec<WorkspaceSummary>> {
            Ok(self
                .workspaces
                .read()
                .await
                .values()
                .cloned()
                .map(WorkspaceSummary::from)
                .collect())
        }

        async fn delete(&self, uuid: &str) -> Result<()> {
            self.workspaces.write().await.remove(uuid);
            Ok(())
        }

        async fn update_accessed(&self, uuid: &str) -> Result<()> {
            if let Some(ws) = self.workspaces.write().await.get_mut(uuid) {
                ws.last_accessed = Utc::now();
            }
            Ok(())
        }

        async fn gc(&self, _max_age_days: u64) -> Result<u64> {
            Ok(0)
        }

        async fn load_config(&self, _uuid: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    struct MockSessionStore;

    #[async_trait::async_trait]
    impl SessionStore for MockSessionStore {
        async fn read(&self, _ws: &str) -> Result<SessionData> {
            Ok(SessionData::new("dummy", "dummy"))
        }
        async fn append(&self, _ws: &str, _msg: &Message) -> Result<()> {
            Ok(())
        }
        async fn write(&self, _ws: &str, _data: &SessionData) -> Result<()> {
            Ok(())
        }
        async fn clear(&self, _ws: &str) -> Result<()> {
            Ok(())
        }
    }

    struct MockBindingStore {
        bindings: RwLock<HashMap<String, String>>,
    }

    impl MockBindingStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                bindings: RwLock::new(HashMap::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl BindingStore for MockBindingStore {
        async fn resolve(&self, device_id: &str) -> Result<Option<String>> {
            Ok(self.bindings.read().await.get(device_id).cloned())
        }
        async fn bind(&self, device_id: &str, workspace_id: &str) -> Result<()> {
            self.bindings
                .write()
                .await
                .insert(device_id.to_string(), workspace_id.to_string());
            Ok(())
        }
        async fn unbind(&self, device_id: &str) -> Result<()> {
            self.bindings.write().await.remove(device_id);
            Ok(())
        }
        async fn unbind_workspace(&self, workspace_id: &str) -> Result<()> {
            self.bindings
                .write()
                .await
                .retain(|_, v| v != workspace_id);
            Ok(())
        }
    }

    struct DummyProvider;

    #[async_trait::async_trait]
    impl LlmProvider for DummyProvider {
        fn name(&self) -> &str {
            "claude"
        }
        async fn complete(
            &self,
            _msgs: &[Message],
            _tools: &[ToolDefinition],
            _cb: Option<crate::provider::StreamCallback>,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: "ok".to_string(),
                stop_reason: StopReason::EndTurn,
                usage: crate::provider::TokenUsage::default(),
                tool_calls: vec![],
            })
        }
        fn model_info(&self) -> ModelInfo {
            ModelInfo {
                name: "claude-sonnet-4-20250514".to_string(),
                max_context_tokens: 200_000,
                max_output_tokens: 8192,
                provider_name: "claude".to_string(),
            }
        }
    }

    fn make_runtime(ws_store: Arc<MockWorkspaceStore>) -> AgentRuntime {
        AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider))
            .with_workspace_store(ws_store)
            .with_session_store(Arc::new(MockSessionStore))
            .with_binding_store(MockBindingStore::new())
            .build()
            .unwrap()
    }

    fn make_service(ws_store: Arc<MockWorkspaceStore>) -> SessionService {
        let workspace_manager = WorkspaceManager::new(
            ws_store,
            Arc::new(MockSessionStore),
            MockBindingStore::new(),
        );
        SessionService::new(&workspace_manager)
    }

    // -- Tests -------------------------------------------------------------

    #[tokio::test]
    async fn resolve_workspace_creates_when_session_id_unknown() {
        let ws_store = MockWorkspaceStore::new();
        let svc = make_service(ws_store.clone());
        let runtime = make_runtime(ws_store);

        let (ws, reusing) = svc
            .resolve_workspace(Some("session-abc"), None, &runtime)
            .await
            .unwrap();

        assert_eq!(ws.uuid, "session-abc");
        assert!(!reusing);
    }

    #[tokio::test]
    async fn resolve_workspace_reuses_existing() {
        let ws_store = MockWorkspaceStore::new();
        // Pre-create the workspace
        ws_store
            .create("session-abc", None, None)
            .await
            .unwrap();

        let svc = make_service(ws_store.clone());
        let runtime = make_runtime(ws_store);

        let (ws, reusing) = svc
            .resolve_workspace(Some("session-abc"), None, &runtime)
            .await
            .unwrap();

        assert_eq!(ws.uuid, "session-abc");
        assert!(reusing);
    }

    #[tokio::test]
    async fn resolve_workspace_falls_back_to_device_id() {
        let ws_store = MockWorkspaceStore::new();
        let svc = make_service(ws_store.clone());
        let runtime = make_runtime(ws_store);

        let (ws, reusing) = svc
            .resolve_workspace(None, Some("my-device"), &runtime)
            .await
            .unwrap();

        assert!(!ws.uuid.is_empty());
        assert!(!reusing);
    }

    #[tokio::test]
    async fn prepare_session_returns_provider_and_model() {
        let ws_store = MockWorkspaceStore::new();
        let svc = make_service(ws_store.clone());
        let runtime = make_runtime(ws_store.clone());

        let workspace = ws_store.create("ws1", None, None).await.unwrap();
        let ctx = svc
            .prepare_session(&workspace, &AgentConfig::default(), &runtime)
            .await
            .unwrap();

        assert!(!ctx.provider_name.is_empty());
        assert!(!ctx.model.is_empty());
        assert!(ctx.max_tokens > 0);
    }

    #[tokio::test]
    async fn prepare_session_uses_workspace_provider_override() {
        let ws_store = MockWorkspaceStore::new();
        let svc = make_service(ws_store.clone());
        let runtime = make_runtime(ws_store.clone());

        // Workspace with explicit provider override
        let mut workspace = ws_store.create("ws2", None, Some("openai")).await.unwrap();
        workspace.provider = Some("openai".to_string());

        let ctx = svc
            .prepare_session(&workspace, &AgentConfig::default(), &runtime)
            .await
            .unwrap();

        assert_eq!(ctx.provider_name, "openai");
    }
}
