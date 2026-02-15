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
}
