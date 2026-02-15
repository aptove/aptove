//! Plugin Trait System
//!
//! Defines the `LlmProvider` and `Plugin` traits, supporting types, and
//! the `PluginHost` registry for compile-time provider selection with
//! runtime dispatch based on TOML config.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// LLM types
// ---------------------------------------------------------------------------

/// Information about a model offered by a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Model identifier (e.g. "claude-sonnet-4-20250514").
    pub name: String,
    /// Maximum input context tokens.
    pub max_context_tokens: usize,
    /// Maximum output tokens per response.
    pub max_output_tokens: usize,
    /// Provider name (e.g. "claude", "gemini", "openai").
    pub provider_name: String,
}

/// Token usage for a single LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
    /// Estimated cost in USD (0.0 if unknown).
    pub estimated_cost_usd: f64,
}

/// Why the LLM stopped generating.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Error,
}

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Unique tool-call id assigned by the provider.
    pub id: String,
    /// Name of the tool to invoke.
    pub name: String,
    /// JSON arguments to pass to the tool.
    pub arguments: serde_json::Value,
}

/// Result returned from executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// The tool call id this result corresponds to.
    pub tool_call_id: String,
    /// The tool's output content.
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

/// Message role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message content — text, tool calls, or tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(ToolCallResult),
}

impl MessageContent {
    /// Rough character count for token estimation.
    pub fn char_len(&self) -> usize {
        match self {
            MessageContent::Text(t) => t.len(),
            MessageContent::ToolCalls(calls) => {
                calls.iter().map(|c| c.name.len() + c.arguments.to_string().len()).sum()
            }
            MessageContent::ToolResult(r) => r.content.len(),
        }
    }
}

/// A tool definition advertised to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
}

/// Response from an LLM provider `complete()` call.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// Text content (may be empty if only tool calls).
    pub content: String,
    /// Tool calls requested by the LLM.
    pub tool_calls: Vec<ToolCallRequest>,
    /// Why the model stopped.
    pub stop_reason: StopReason,
    /// Token usage.
    pub usage: TokenUsage,
}

// ---------------------------------------------------------------------------
// Streaming callback
// ---------------------------------------------------------------------------

/// Callback invoked for each streamed chunk from the LLM.
pub type StreamCallback = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// Events emitted during streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text chunk from the LLM.
    TextDelta(String),
    /// A tool call has started.
    ToolCallStart {
        id: String,
        name: String,
    },
    /// A tool call argument chunk.
    ToolCallDelta {
        id: String,
        arguments_delta: String,
    },
}

/// A chunk of output flowing to the user. Plugins can inspect or mutate it
/// via the `on_output_chunk` hook.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    /// The text content of the chunk.
    pub text: String,
    /// Whether this chunk is the final one in the current assistant turn.
    pub is_final: bool,
}

// ---------------------------------------------------------------------------
// LLM Provider trait
// ---------------------------------------------------------------------------

/// Trait implemented by each LLM provider (Claude, Gemini, OpenAI).
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Provider identifier (e.g. "claude", "gemini", "openai").
    fn name(&self) -> &str;

    /// Send a completion request. Invokes `stream_cb` for each chunk
    /// as it arrives, then returns the full aggregated response.
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream_cb: Option<StreamCallback>,
    ) -> Result<LlmResponse>;

    /// Estimate token count for a text string.
    /// Providers should override with their exact tokenizer.
    fn count_tokens(&self, text: &str) -> usize {
        // Default: chars / 4 heuristic
        text.len() / 4
    }

    /// Return metadata about the active model.
    fn model_info(&self) -> ModelInfo;
}

// ---------------------------------------------------------------------------
// Plugin trait (lifecycle)
// ---------------------------------------------------------------------------

/// General plugin trait for lifecycle hooks.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Unique plugin name.
    fn name(&self) -> &str;

    /// Called once during agent startup. Receives the plugin's config section.
    async fn on_init(&mut self, config: &serde_json::Value) -> Result<()> {
        let _ = config;
        Ok(())
    }

    /// Called during graceful shutdown.
    async fn on_shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Called before a tool call is dispatched. Plugins can inspect or
    /// mutate the request (e.g. inject arguments, block calls).
    async fn on_before_tool_call(&self, _call: &mut ToolCallRequest) -> Result<()> {
        Ok(())
    }

    /// Called after a tool call completes. Plugins can inspect the request
    /// and mutate the result (e.g. post-process output, record undo state).
    async fn on_after_tool_call(
        &self,
        _call: &ToolCallRequest,
        _result: &mut ToolCallResult,
    ) -> Result<()> {
        Ok(())
    }

    /// Called for each chunk of output flowing to the user. Plugins can
    /// mutate the chunk (e.g. apply formatting, syntax highlighting).
    async fn on_output_chunk(&self, _chunk: &mut OutputChunk) -> Result<()> {
        Ok(())
    }

    /// Called when the context window is about to compact. If a plugin
    /// returns `Some(messages)` those replace the dropped messages (e.g.
    /// with a summary). The first plugin to return `Some` wins.
    async fn on_context_compact(
        &self,
        _messages: &[Message],
        _target_tokens: usize,
    ) -> Result<Option<Vec<Message>>> {
        Ok(None)
    }

    /// Called when a new session starts. Plugins can inject initial
    /// context, warn about state, etc.
    async fn on_session_start(&self, _session: &mut crate::session::Session) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Plugin Host
// ---------------------------------------------------------------------------

/// Registry of loaded plugins and LLM providers.
pub struct PluginHost {
    /// All registered plugins (lifecycle hooks).
    plugins: Vec<Box<dyn Plugin>>,
    /// LLM providers keyed by name.
    providers: HashMap<String, Arc<RwLock<Box<dyn LlmProvider>>>>,
    /// Currently active provider name.
    active_provider: Option<String>,
    /// Tool definitions from MCP servers.
    tools: Vec<ToolDefinition>,
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            providers: HashMap::new(),
            active_provider: None,
            tools: Vec::new(),
        }
    }

    /// Register a plugin for lifecycle hooks.
    pub fn register_plugin(&mut self, plugin: Box<dyn Plugin>) {
        tracing::info!(name = plugin.name(), "registered plugin");
        self.plugins.push(plugin);
    }

    /// Register an LLM provider. The first registered provider becomes active.
    pub fn register_provider(&mut self, provider: Box<dyn LlmProvider>) {
        let name = provider.name().to_string();
        tracing::info!(name = %name, "registered LLM provider");
        if self.active_provider.is_none() {
            self.active_provider = Some(name.clone());
        }
        self.providers
            .insert(name, Arc::new(RwLock::new(provider)));
    }

    /// Set the active provider by name. Returns error if not found.
    pub fn set_active_provider(&mut self, name: &str) -> Result<()> {
        if self.providers.contains_key(name) {
            self.active_provider = Some(name.to_string());
            Ok(())
        } else {
            anyhow::bail!(
                "provider '{}' not found (available: {:?})",
                name,
                self.providers.keys().collect::<Vec<_>>()
            );
        }
    }

    /// Get the active LLM provider.
    pub fn active_provider(&self) -> Result<Arc<RwLock<Box<dyn LlmProvider>>>> {
        let name = self
            .active_provider
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active provider"))?;
        self.providers
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", name))
    }

    /// Get the active provider name.
    pub fn active_provider_name(&self) -> Option<&str> {
        self.active_provider.as_deref()
    }

    /// List registered provider names.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }

    /// Register tools (typically from MCP servers).
    pub fn register_tools(&mut self, new_tools: Vec<ToolDefinition>) {
        self.tools.extend(new_tools);
    }

    /// Get all available tool definitions.
    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Remove tools by source (e.g. when an MCP server crashes).
    pub fn remove_tools_by_prefix(&mut self, prefix: &str) {
        self.tools.retain(|t| !t.name.starts_with(prefix));
    }

    /// Initialize all plugins.
    pub async fn init_all(&mut self, config: &serde_json::Value) -> Result<()> {
        for plugin in &mut self.plugins {
            plugin.on_init(config).await?;
        }
        Ok(())
    }

    /// Shut down all plugins.
    pub async fn shutdown_all(&self) -> Result<()> {
        for plugin in &self.plugins {
            if let Err(e) = plugin.on_shutdown().await {
                tracing::warn!(plugin = plugin.name(), err = %e, "plugin shutdown error");
            }
        }
        Ok(())
    }

    // -- Hook dispatch ------------------------------------------------------

    /// Run `on_before_tool_call` on all plugins in registration order.
    pub async fn run_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_before_tool_call(call).await?;
        }
        Ok(())
    }

    /// Run `on_after_tool_call` on all plugins in registration order.
    pub async fn run_after_tool_call(
        &self,
        call: &ToolCallRequest,
        result: &mut ToolCallResult,
    ) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_after_tool_call(call, result).await?;
        }
        Ok(())
    }

    /// Run `on_output_chunk` on all plugins in registration order.
    pub async fn run_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()> {
        for plugin in &self.plugins {
            plugin.on_output_chunk(chunk).await?;
        }
        Ok(())
    }

    /// Run `on_context_compact` on all plugins. Returns the first `Some`
    /// result from any plugin, or `None` if no plugin overrides.
    pub async fn run_context_compact(
        &self,
        messages: &[Message],
        target_tokens: usize,
    ) -> Result<Option<Vec<Message>>> {
        for plugin in &self.plugins {
            if let Some(replacement) = plugin.on_context_compact(messages, target_tokens).await? {
                return Ok(Some(replacement));
            }
        }
        Ok(None)
    }

    /// Run `on_session_start` on all plugins in registration order.
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

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyProvider;

    #[async_trait]
    impl LlmProvider for DummyProvider {
        fn name(&self) -> &str {
            "dummy"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _stream_cb: Option<StreamCallback>,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                content: "Hello!".into(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::default(),
            })
        }
        fn model_info(&self) -> ModelInfo {
            ModelInfo {
                name: "dummy-model".into(),
                max_context_tokens: 4096,
                max_output_tokens: 1024,
                provider_name: "dummy".into(),
            }
        }
    }

    #[test]
    fn plugin_host_register_and_lookup() {
        let mut host = PluginHost::new();
        host.register_provider(Box::new(DummyProvider));

        assert_eq!(host.active_provider_name(), Some("dummy"));
        assert!(host.active_provider().is_ok());
        assert_eq!(host.provider_names(), vec!["dummy"]);
    }

    #[test]
    fn plugin_host_switch_provider() {
        let mut host = PluginHost::new();
        host.register_provider(Box::new(DummyProvider));

        assert!(host.set_active_provider("nonexistent").is_err());
        assert!(host.set_active_provider("dummy").is_ok());
    }

    #[test]
    fn default_token_count() {
        let provider = DummyProvider;
        // "hello world" = 11 chars → 11/4 = 2
        assert_eq!(provider.count_tokens("hello world"), 2);
    }

    // -----------------------------------------------------------------------
    // Hook tests
    // -----------------------------------------------------------------------

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A test plugin that records which hooks were called and in what order,
    /// and optionally mutates values to prove the hooks have effect.
    struct RecordingPlugin {
        plugin_name: String,
        /// Shared counter to verify registration-order execution.
        call_order: Arc<AtomicUsize>,
        /// The order value assigned when `on_before_tool_call` fires.
        before_order: Arc<std::sync::Mutex<Option<usize>>>,
        /// The order value assigned when `on_after_tool_call` fires.
        after_order: Arc<std::sync::Mutex<Option<usize>>>,
        /// The order value assigned when `on_output_chunk` fires.
        output_order: Arc<std::sync::Mutex<Option<usize>>>,
        /// The order value assigned when `on_session_start` fires.
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

    #[async_trait]
    impl Plugin for RecordingPlugin {
        fn name(&self) -> &str {
            &self.plugin_name
        }

        async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.before_order.lock().unwrap() = Some(order);
            // Mutate: prepend plugin name to tool arguments
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
        ) -> Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.after_order.lock().unwrap() = Some(order);
            // Mutate: append plugin name to result content
            result.content.push_str(&format!(" [after:{}]", self.plugin_name));
            Ok(())
        }

        async fn on_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.output_order.lock().unwrap() = Some(order);
            // Mutate: wrap text
            chunk.text = format!("[{}]{}", self.plugin_name, chunk.text);
            Ok(())
        }

        async fn on_session_start(
            &self,
            _session: &mut crate::session::Session,
        ) -> Result<()> {
            let order = self.call_order.fetch_add(1, Ordering::SeqCst);
            *self.session_start_order.lock().unwrap() = Some(order);
            Ok(())
        }
    }

    /// Plugin that returns a compacted summary from `on_context_compact`.
    struct SummarizerPlugin;

    #[async_trait]
    impl Plugin for SummarizerPlugin {
        fn name(&self) -> &str {
            "summarizer"
        }

        async fn on_context_compact(
            &self,
            messages: &[Message],
            _target_tokens: usize,
        ) -> Result<Option<Vec<Message>>> {
            // Replace all messages with a single summary
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

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(plugin_a));
        host.register_plugin(Box::new(plugin_b));

        // Run on_before_tool_call
        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        };
        host.run_before_tool_call(&mut call).await.unwrap();

        // Alpha should fire before beta
        assert_eq!(*a_before.lock().unwrap(), Some(0));
        assert_eq!(*b_before.lock().unwrap(), Some(1));

        // Run on_after_tool_call
        let mut result = ToolCallResult {
            tool_call_id: "tc1".into(),
            content: "output".into(),
            is_error: false,
        };
        host.run_after_tool_call(&call, &mut result).await.unwrap();

        assert_eq!(*a_after.lock().unwrap(), Some(2));
        assert_eq!(*b_after.lock().unwrap(), Some(3));
    }

    #[tokio::test]
    async fn before_tool_call_mutates_request() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("mutator", counter);

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(plugin));

        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/foo"}),
        };
        host.run_before_tool_call(&mut call).await.unwrap();

        // Arguments should be wrapped
        assert_eq!(call.arguments["modified_by"], "mutator");
        assert_eq!(call.arguments["original"]["path"], "/tmp/foo");
    }

    #[tokio::test]
    async fn after_tool_call_mutates_result() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("postproc", counter);

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(plugin));

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
        host.run_after_tool_call(&call, &mut result).await.unwrap();

        assert_eq!(result.content, "file.txt [after:postproc]");
    }

    #[tokio::test]
    async fn output_chunk_hook_mutates_text() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin_a = RecordingPlugin::new("A", counter.clone());
        let plugin_b = RecordingPlugin::new("B", counter);

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(plugin_a));
        host.register_plugin(Box::new(plugin_b));

        let mut chunk = OutputChunk {
            text: "hello".into(),
            is_final: false,
        };
        host.run_output_chunk(&mut chunk).await.unwrap();

        // B wraps A's output: [B][A]hello
        assert_eq!(chunk.text, "[B][A]hello");
    }

    #[tokio::test]
    async fn context_compact_first_plugin_wins() {
        let mut host = PluginHost::new();

        // Register a summarizer — it returns Some
        host.register_plugin(Box::new(SummarizerPlugin));

        let messages = vec![
            Message { role: Role::User, content: MessageContent::Text("msg1".into()) },
            Message { role: Role::User, content: MessageContent::Text("msg2".into()) },
            Message { role: Role::User, content: MessageContent::Text("msg3".into()) },
        ];

        let result = host.run_context_compact(&messages, 100).await.unwrap();
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
        // A host with no plugins that override on_context_compact
        let counter = Arc::new(AtomicUsize::new(0));
        let mut host = PluginHost::new();
        host.register_plugin(Box::new(RecordingPlugin::new("noop", counter)));

        let messages = vec![
            Message { role: Role::User, content: MessageContent::Text("m1".into()) },
        ];
        let result = host.run_context_compact(&messages, 100).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn session_start_hook_called() {
        let counter = Arc::new(AtomicUsize::new(0));
        let plugin = RecordingPlugin::new("starter", counter.clone());
        let order = plugin.session_start_order.clone();

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(plugin));

        let mut session = crate::session::Session::new(4096, "claude", "sonnet");
        host.run_session_start(&mut session).await.unwrap();

        assert_eq!(*order.lock().unwrap(), Some(0));
    }

    #[tokio::test]
    async fn default_noop_hooks_pass_through() {
        // A plugin with all default hooks (no overrides)
        struct NoopPlugin;
        #[async_trait]
        impl Plugin for NoopPlugin {
            fn name(&self) -> &str { "noop" }
        }

        let mut host = PluginHost::new();
        host.register_plugin(Box::new(NoopPlugin));

        // on_before_tool_call — arguments should be unchanged
        let mut call = ToolCallRequest {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "echo hi"}),
        };
        let original_args = call.arguments.clone();
        host.run_before_tool_call(&mut call).await.unwrap();
        assert_eq!(call.arguments, original_args);

        // on_after_tool_call — result should be unchanged
        let mut result = ToolCallResult {
            tool_call_id: "tc1".into(),
            content: "output".into(),
            is_error: false,
        };
        host.run_after_tool_call(&call, &mut result).await.unwrap();
        assert_eq!(result.content, "output");

        // on_output_chunk — text should be unchanged
        let mut chunk = OutputChunk {
            text: "hello world".into(),
            is_final: true,
        };
        host.run_output_chunk(&mut chunk).await.unwrap();
        assert_eq!(chunk.text, "hello world");

        // on_context_compact — should return None
        let msgs = vec![Message { role: Role::User, content: MessageContent::Text("x".into()) }];
        let compact_result = host.run_context_compact(&msgs, 100).await.unwrap();
        assert!(compact_result.is_none());

        // on_session_start — should succeed without error
        let mut session = crate::session::Session::new(4096, "test", "model");
        host.run_session_start(&mut session).await.unwrap();
    }
}
