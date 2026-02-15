//! Agent Builder and Runtime
//!
//! `AgentBuilder` is the typed builder for constructing an agent with all
//! extension points (LLM providers, storage backends, plugins).
//! `AgentRuntime` holds the resolved services at runtime.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use crate::bindings::BindingStore;
use crate::config::AgentConfig;
use crate::persistence::SessionStore;
use crate::plugin::{LlmProvider, Plugin, ToolDefinition};
use crate::workspace::{WorkspaceManager, WorkspaceStore};

// ---------------------------------------------------------------------------
// AgentBuilder
// ---------------------------------------------------------------------------

/// Typed builder for constructing an agent.
///
/// Each extension point is an explicit `.with_*()` method. A plugin author
/// reads the builder methods to discover what they can implement.
pub struct AgentBuilder {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    active_provider: Option<String>,
    workspace_store: Option<Arc<dyn WorkspaceStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    binding_store: Option<Arc<dyn BindingStore>>,
    plugins: Vec<Box<dyn Plugin>>,
    config: AgentConfig,
}

impl AgentBuilder {
    /// Create a new builder with the given base configuration.
    pub fn new(config: AgentConfig) -> Self {
        Self {
            providers: HashMap::new(),
            active_provider: None,
            workspace_store: None,
            session_store: None,
            binding_store: None,
            plugins: Vec::new(),
            config,
        }
    }

    /// Register an LLM provider. The first registered provider becomes active.
    pub fn with_llm(mut self, provider: Arc<dyn LlmProvider>) -> Self {
        let name = provider.name().to_string();
        if self.active_provider.is_none() {
            self.active_provider = Some(name.clone());
        }
        self.providers.insert(name, provider);
        self
    }

    /// Set the workspace storage backend.
    pub fn with_workspace_store(mut self, store: Arc<dyn WorkspaceStore>) -> Self {
        self.workspace_store = Some(store);
        self
    }

    /// Set the session storage backend.
    pub fn with_session_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.session_store = Some(store);
        self
    }

    /// Set the device-binding storage backend.
    pub fn with_binding_store(mut self, store: Arc<dyn BindingStore>) -> Self {
        self.binding_store = Some(store);
        self
    }

    /// Register a lifecycle plugin (e.g., MCP bridge).
    pub fn with_plugin(mut self, plugin: Box<dyn Plugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Override the active provider name.
    pub fn with_active_provider(mut self, name: &str) -> Self {
        self.active_provider = Some(name.to_string());
        self
    }

    /// Validate required slots and construct the `AgentRuntime`.
    pub fn build(self) -> Result<AgentRuntime> {
        let ws = self
            .workspace_store
            .ok_or_else(|| anyhow::anyhow!("workspace_store is required"))?;
        let ss = self
            .session_store
            .ok_or_else(|| anyhow::anyhow!("session_store is required"))?;
        let bs = self
            .binding_store
            .ok_or_else(|| anyhow::anyhow!("binding_store is required"))?;

        if self.providers.is_empty() {
            anyhow::bail!("at least one LLM provider is required");
        }

        let active = self
            .active_provider
            .or_else(|| self.providers.keys().next().cloned())
            .ok_or_else(|| anyhow::anyhow!("no active provider set"))?;

        if !self.providers.contains_key(&active) {
            anyhow::bail!("active provider '{}' not registered", active);
        }

        let workspace_manager = WorkspaceManager::new(ws, ss, bs);

        Ok(AgentRuntime {
            providers: self.providers,
            active_provider: active,
            workspace_manager,
            plugins: self.plugins,
            config: self.config,
            tools: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------------------
// AgentRuntime
// ---------------------------------------------------------------------------

/// Holds the fully constructed agent services at runtime.
///
/// Replaces `PluginHost` for runtime service access.
pub struct AgentRuntime {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
    active_provider: String,
    workspace_manager: WorkspaceManager,
    plugins: Vec<Box<dyn Plugin>>,
    config: AgentConfig,
    tools: Vec<ToolDefinition>,
}

impl AgentRuntime {
    /// Get the active LLM provider.
    pub fn active_provider(&self) -> Result<Arc<dyn LlmProvider>> {
        self.providers
            .get(&self.active_provider)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("active provider '{}' not found", self.active_provider))
    }

    /// Get the active provider name.
    pub fn active_provider_name(&self) -> &str {
        &self.active_provider
    }

    /// Switch the active provider.
    pub fn set_active_provider(&mut self, name: &str) -> Result<()> {
        if self.providers.contains_key(name) {
            self.active_provider = name.to_string();
            Ok(())
        } else {
            anyhow::bail!(
                "provider '{}' not found (available: {:?})",
                name,
                self.providers.keys().collect::<Vec<_>>()
            );
        }
    }

    /// List registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// Access the workspace manager.
    pub fn workspace_manager(&self) -> &WorkspaceManager {
        &self.workspace_manager
    }

    /// Access the agent configuration.
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    /// Get all available tool definitions.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Register tools (typically from MCP servers).
    pub fn register_tools(&mut self, new_tools: Vec<ToolDefinition>) {
        self.tools.extend(new_tools);
    }

    /// Remove tools by name prefix.
    pub fn remove_tools_by_prefix(&mut self, prefix: &str) {
        self.tools.retain(|t| !t.name.starts_with(prefix));
    }

    /// Initialize all lifecycle plugins.
    pub async fn init_plugins(&mut self) -> Result<()> {
        let config_val = serde_json::to_value(&self.config)?;
        for plugin in &mut self.plugins {
            plugin.on_init(&config_val).await?;
        }
        Ok(())
    }

    /// Shut down all lifecycle plugins.
    pub async fn shutdown_plugins(&self) -> Result<()> {
        for plugin in &self.plugins {
            if let Err(e) = plugin.on_shutdown().await {
                tracing::warn!(plugin = plugin.name(), err = %e, "plugin shutdown error");
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::BindingStore;
    use crate::persistence::{SessionData, SessionStore};
    use crate::plugin::{
        LlmProvider, LlmResponse, Message, ModelInfo, StopReason, StreamCallback, ToolDefinition,
        TokenUsage,
    };

    // -- Mock implementations ----------------------------------------------

    struct DummyProvider {
        name: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for DummyProvider {
        fn name(&self) -> &str {
            &self.name
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _stream_cb: Option<StreamCallback>,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: "ok".into(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
        fn model_info(&self) -> ModelInfo {
            ModelInfo {
                name: "dummy".into(),
                max_context_tokens: 4096,
                max_output_tokens: 1024,
                provider_name: self.name.clone(),
            }
        }
    }

    struct MockWS;
    #[async_trait::async_trait]
    impl WorkspaceStore for MockWS {
        async fn create(&self, uuid: &str, name: Option<&str>, _p: Option<&str>) -> Result<crate::workspace::WorkspaceMetadata> {
            Ok(crate::workspace::WorkspaceMetadata {
                uuid: uuid.into(),
                name: name.map(|s| s.into()),
                created_at: chrono::Utc::now(),
                last_accessed: chrono::Utc::now(),
                provider: None,
            })
        }
        async fn load(&self, uuid: &str) -> Result<crate::workspace::WorkspaceMetadata> {
            Ok(crate::workspace::WorkspaceMetadata {
                uuid: uuid.into(),
                name: None,
                created_at: chrono::Utc::now(),
                last_accessed: chrono::Utc::now(),
                provider: None,
            })
        }
        async fn list(&self) -> Result<Vec<crate::workspace::WorkspaceSummary>> { Ok(vec![]) }
        async fn delete(&self, _: &str) -> Result<()> { Ok(()) }
        async fn update_accessed(&self, _: &str) -> Result<()> { Ok(()) }
        async fn gc(&self, _: u64) -> Result<u64> { Ok(0) }
        async fn load_config(&self, _: &str) -> Result<Option<String>> { Ok(None) }
    }

    struct MockSS;
    #[async_trait::async_trait]
    impl SessionStore for MockSS {
        async fn read(&self, _: &str) -> Result<SessionData> { Ok(SessionData::new("m", "m")) }
        async fn append(&self, _: &str, _: &Message) -> Result<()> { Ok(()) }
        async fn write(&self, _: &str, _: &SessionData) -> Result<()> { Ok(()) }
        async fn clear(&self, _: &str) -> Result<()> { Ok(()) }
    }

    struct MockBS;
    #[async_trait::async_trait]
    impl BindingStore for MockBS {
        async fn resolve(&self, _: &str) -> Result<Option<String>> { Ok(None) }
        async fn bind(&self, _: &str, _: &str) -> Result<()> { Ok(()) }
        async fn unbind(&self, _: &str) -> Result<()> { Ok(()) }
        async fn unbind_workspace(&self, _: &str) -> Result<()> { Ok(()) }
    }

    #[test]
    fn build_requires_stores() {
        let config = AgentConfig::default();
        let result = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "dummy".into() }))
            .build();
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("workspace_store"));
    }

    #[test]
    fn build_requires_provider() {
        let config = AgentConfig::default();
        let result = AgentBuilder::new(config)
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build();
        let err = result.err().expect("expected error");
        assert!(err.to_string().contains("provider"));
    }

    #[test]
    fn build_success() {
        let config = AgentConfig::default();
        let runtime = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "dummy".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap();

        assert_eq!(runtime.active_provider_name(), "dummy");
        assert!(runtime.active_provider().is_ok());
    }

    #[test]
    fn build_with_active_provider() {
        let config = AgentConfig::default();
        let runtime = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "alpha".into() }))
            .with_llm(Arc::new(DummyProvider { name: "beta".into() }))
            .with_active_provider("beta")
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap();

        assert_eq!(runtime.active_provider_name(), "beta");
    }

    #[test]
    fn build_invalid_active_provider() {
        let config = AgentConfig::default();
        let result = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "alpha".into() }))
            .with_active_provider("nonexistent")
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn runtime_switch_provider() {
        let config = AgentConfig::default();
        let mut runtime = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "alpha".into() }))
            .with_llm(Arc::new(DummyProvider { name: "beta".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap();

        assert!(runtime.set_active_provider("beta").is_ok());
        assert_eq!(runtime.active_provider_name(), "beta");
        assert!(runtime.set_active_provider("nonexistent").is_err());
    }

    #[test]
    fn runtime_tools() {
        let config = AgentConfig::default();
        let mut runtime = AgentBuilder::new(config)
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap();

        assert!(runtime.tools().is_empty());
        runtime.register_tools(vec![ToolDefinition {
            name: "bash_exec".into(),
            description: "Execute bash".into(),
            parameters: serde_json::json!({}),
        }]);
        assert_eq!(runtime.tools().len(), 1);
        runtime.remove_tools_by_prefix("bash_");
        assert!(runtime.tools().is_empty());
    }
}
