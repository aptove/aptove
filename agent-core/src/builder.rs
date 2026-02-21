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
use crate::plugin::{OutputChunk, Plugin};
use crate::provider::LlmProvider;
use crate::scheduler::SchedulerStore;
use crate::trigger::TriggerStore;
use crate::types::ToolDefinition;
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
    scheduler_store: Option<Arc<dyn SchedulerStore>>,
    trigger_store: Option<Arc<dyn TriggerStore>>,
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
            scheduler_store: None,
            trigger_store: None,
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

    /// Set the scheduler storage backend.
    /// If not provided, the scheduler is disabled.
    pub fn with_scheduler_store(mut self, store: Arc<dyn SchedulerStore>) -> Self {
        self.scheduler_store = Some(store);
        self
    }

    /// Set the webhook trigger storage backend.
    /// If not provided, webhook triggers are disabled.
    pub fn with_trigger_store(mut self, store: Arc<dyn TriggerStore>) -> Self {
        self.trigger_store = Some(store);
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
            scheduler_store: self.scheduler_store,
            trigger_store: self.trigger_store,
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
    scheduler_store: Option<Arc<dyn SchedulerStore>>,
    trigger_store: Option<Arc<dyn TriggerStore>>,
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

    /// Access the scheduler store (if configured).
    pub fn scheduler_store(&self) -> Option<&Arc<dyn SchedulerStore>> {
        self.scheduler_store.as_ref()
    }

    /// Access the trigger store (if configured).
    pub fn trigger_store(&self) -> Option<&Arc<dyn TriggerStore>> {
        self.trigger_store.as_ref()
    }

    /// Access all registered LLM providers.
    pub fn providers(&self) -> &HashMap<String, Arc<dyn LlmProvider>> {
        &self.providers
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

    /// Run `on_before_tool_call` on all plugins.
    pub async fn run_before_tool_call(&self, call: &mut crate::types::ToolCallRequest) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_before_tool_call(call).await?;
        }
        Ok(())
    }

    /// Run `on_after_tool_call` on all plugins.
    pub async fn run_after_tool_call(&self, call: &crate::types::ToolCallRequest, result: &mut crate::types::ToolCallResult) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_after_tool_call(call, result).await?;
        }
        Ok(())
    }

    /// Run `on_output_chunk` on all plugins.
    pub async fn run_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_output_chunk(chunk).await?;
        }
        Ok(())
    }

    /// Run `on_context_compact` on all plugins. Returns first Some() result.
    pub async fn run_context_compact(&self, messages: &[crate::types::Message], target_tokens: usize) -> Result<Option<Vec<crate::types::Message>>> {
        for plugin in &self.plugins {
            if let Some(replacement) = plugin.on_context_compact(messages, target_tokens).await? {
                return Ok(Some(replacement));
            }
        }
        Ok(None)
    }

    /// Run `on_session_start` on all plugins.
    pub async fn run_session_start(&self, session: &mut crate::session::Session) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_session_start(session).await?;
        }
        Ok(())
    }

    /// Access registered plugins (for testing/inspection).
    pub fn plugins(&self) -> &[Box<dyn Plugin>] {
        &self.plugins
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
    use crate::provider::{LlmProvider, LlmResponse, ModelInfo, StopReason, StreamCallback, TokenUsage};
    use crate::types::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult, ToolDefinition};

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

    // -----------------------------------------------------------------------
    // Hook dispatch tests (moved from plugin.rs)
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};
    use crate::plugin::{OutputChunk, Plugin};

    fn build_test_runtime() -> AgentRuntime {
        AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap()
    }

    struct RecordingPlugin {
        plugin_name: String,
        call_order: Arc<AtomicUsize>,
        before_order: Arc<std::sync::Mutex<Option<usize>>>,
        after_order: Arc<std::sync::Mutex<Option<usize>>>,
        output_order: Arc<std::sync::Mutex<Option<usize>>>,
        session_start_order: Arc<std::sync::Mutex<Option<usize>>>,
    }

    impl RecordingPlugin {
        fn new(name: &str, counter: Arc<AtomicUsize>) -> Self {
            Self {
                plugin_name: name.to_string(),
                call_order: counter,
                before_order: Arc::new(std::sync::Mutex::new(None)),
                after_order: Arc::new(std::sync::Mutex::new(None)),
                output_order: Arc::new(std::sync::Mutex::new(None)),
                session_start_order: Arc::new(std::sync::Mutex::new(None)),
            }
        }
    }

    #[async_trait::async_trait]
    impl Plugin for RecordingPlugin {
        fn name(&self) -> &str {
            &self.plugin_name
        }

        async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> anyhow::Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.before_order.lock().unwrap() = Some(order);
            call.arguments = serde_json::json!({
                "modified_by": self.plugin_name,
                "original": call.arguments,
            });
            Ok(())
        }

        async fn on_after_tool_call(
            &self,
            _call: &ToolCallRequest,
            result: &mut ToolCallResult,
        ) -> anyhow::Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.after_order.lock().unwrap() = Some(order);
            result.content.push_str(&format!(" [after:{}]", self.plugin_name));
            Ok(())
        }

        async fn on_output_chunk(&self, chunk: &mut OutputChunk) -> anyhow::Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.output_order.lock().unwrap() = Some(order);
            chunk.text = format!("[{}]{}", self.plugin_name, chunk.text);
            Ok(())
        }

        async fn on_session_start(
            &self,
            _session: &mut crate::session::Session,
        ) -> anyhow::Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.session_start_order.lock().unwrap() = Some(order);
            Ok(())
        }
    }

    struct SummarizerPlugin;

    #[async_trait::async_trait]
    impl Plugin for SummarizerPlugin {
        fn name(&self) -> &str {
            "summarizer"
        }

        async fn on_context_compact(
            &self,
            messages: &[Message],
            _target_tokens: usize,
        ) -> anyhow::Result<Option<Vec<Message>>> {
            let summary = format!("[summary of {} messages]", messages.len());
            Ok(Some(vec![Message {
                role: Role::Assistant,
                content: MessageContent::Text(summary),
            }]))
        }
    }

    #[tokio::test]
    async fn hooks_called_in_registration_order() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin_a = RecordingPlugin::new("alpha", counter.clone());
        let a_before = plugin_a.before_order.clone();
        let a_after = plugin_a.after_order.clone();

        let plugin_b = RecordingPlugin::new("beta", counter.clone());
        let b_before = plugin_b.before_order.clone();
        let b_after = plugin_b.after_order.clone();

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(plugin_a))
            .with_plugin(Box::new(plugin_b))
            .build()
            .unwrap();

        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        };
        runtime.run_before_tool_call(&mut call).await.unwrap();

        assert_eq!(*a_before.lock().unwrap(), Some(0));
        assert_eq!(*b_before.lock().unwrap(), Some(1));

        let mut result = ToolCallResult {
            tool_call_id: "tc1".into(),
            content: "output".into(),
            is_error: false,
        };
        runtime.run_after_tool_call(&call, &mut result).await.unwrap();

        assert_eq!(*a_after.lock().unwrap(), Some(2));
        assert_eq!(*b_after.lock().unwrap(), Some(3));
    }

    #[tokio::test]
    async fn before_tool_call_mutates_request() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("mutator", counter);

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(plugin))
            .build()
            .unwrap();

        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/foo"}),
        };
        runtime.run_before_tool_call(&mut call).await.unwrap();

        assert_eq!(call.arguments["modified_by"], "mutator");
        assert_eq!(call.arguments["original"]["path"], "/tmp/foo");
    }

    #[tokio::test]
    async fn after_tool_call_mutates_result() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("postproc", counter);

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(plugin))
            .build()
            .unwrap();

        let call = ToolCallRequest {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        };
        let mut result = ToolCallResult {
            tool_call_id: "tc1".into(),
            content: "file.txt".into(),
            is_error: false,
        };
        runtime.run_after_tool_call(&call, &mut result).await.unwrap();

        assert_eq!(result.content, "file.txt [after:postproc]");
    }

    #[tokio::test]
    async fn output_chunk_hook_mutates_text() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin_a = RecordingPlugin::new("A", counter.clone());
        let plugin_b = RecordingPlugin::new("B", counter);

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(plugin_a))
            .with_plugin(Box::new(plugin_b))
            .build()
            .unwrap();

        let mut chunk = OutputChunk {
            text: "hello".into(),
            is_final: false,
        };
        runtime.run_output_chunk(&mut chunk).await.unwrap();

        assert_eq!(chunk.text, "[B][A]hello");
    }

    #[tokio::test]
    async fn context_compact_first_plugin_wins() {
        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(SummarizerPlugin))
            .build()
            .unwrap();

        let messages = vec![
            Message { role: Role::User, content: MessageContent::Text("msg1".into()) },
            Message { role: Role::User, content: MessageContent::Text("msg2".into()) },
            Message { role: Role::User, content: MessageContent::Text("msg3".into()) },
        ];

        let result = runtime.run_context_compact(&messages, 100).await.unwrap();
        assert!(result.is_some());
        let replacement = result.unwrap();
        assert_eq!(replacement.len(), 1);
        match &replacement[0].content {
            MessageContent::Text(t) => assert_eq!(t, "[summary of 3 messages]"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn context_compact_no_override_returns_none() {
        let counter = Arc::new(AtomicUsize::new(0));
        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(RecordingPlugin::new("noop", counter)))
            .build()
            .unwrap();

        let messages = vec![
            Message { role: Role::User, content: MessageContent::Text("m1".into()) },
        ];
        let result = runtime.run_context_compact(&messages, 100).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn session_start_hook_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("starter", counter.clone());
        let order = plugin.session_start_order.clone();

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(plugin))
            .build()
            .unwrap();

        let mut session = crate::session::Session::new(4096, "claude", "sonnet");
        runtime.run_session_start(&mut session).await.unwrap();

        assert_eq!(*order.lock().unwrap(), Some(0));
    }

    #[tokio::test]
    async fn default_noop_hooks_pass_through() {
        struct NoopPlugin;
        #[async_trait::async_trait]
        impl Plugin for NoopPlugin {
            fn name(&self) -> &str { "noop" }
        }

        let runtime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider { name: "d".into() }))
            .with_workspace_store(Arc::new(MockWS))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .with_plugin(Box::new(NoopPlugin))
            .build()
            .unwrap();

        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "echo hi"}),
        };
        let original_args = call.arguments.clone();
        runtime.run_before_tool_call(&mut call).await.unwrap();
        assert_eq!(call.arguments, original_args);

        let mut result = ToolCallResult {
            tool_call_id: "tc1".into(),
            content: "output".into(),
            is_error: false,
        };
        runtime.run_after_tool_call(&call, &mut result).await.unwrap();
        assert_eq!(result.content, "output");

        let mut chunk = OutputChunk {
            text: "hello world".into(),
            is_final: true,
        };
        runtime.run_output_chunk(&mut chunk).await.unwrap();
        assert_eq!(chunk.text, "hello world");

        let msgs = vec![Message { role: Role::User, content: MessageContent::Text("x".into()) }];
        let compact_result = runtime.run_context_compact(&msgs, 100).await.unwrap();
        assert!(compact_result.is_none());

        let mut session = crate::session::Session::new(4096, "test", "model");
        runtime.run_session_start(&mut session).await.unwrap();
    }
}
