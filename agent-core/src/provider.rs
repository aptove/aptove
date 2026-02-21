//! LLM Provider Trait and Types
//!
//! Defines the `LlmProvider` trait and supporting types for LLM responses,
//! model information, token usage, and streaming.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{Message, ToolDefinition, ToolCallRequest};

// ---------------------------------------------------------------------------
// LLM response types
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
// Streaming
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


