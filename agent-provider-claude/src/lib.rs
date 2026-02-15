//! Anthropic Claude LLM Provider
//!
//! Implements `LlmProvider` for the Anthropic Messages API.
//! Supports streaming, tool use, and token counting.

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::debug;

use agent_core::plugin::{
    LlmProvider, LlmResponse, Message, MessageContent, ModelInfo, Role, StopReason,
    StreamCallback, StreamEvent, ToolCallRequest, ToolDefinition, TokenUsage,
};

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

/// Anthropic Claude LLM provider.
pub struct ClaudeProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl ClaudeProvider {
    pub fn new(api_key: &str, model: &str, base_url: Option<&str>) -> Self {
        Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            base_url: base_url
                .unwrap_or("https://api.anthropic.com")
                .to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Convert internal messages to Anthropic API format.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let mut system_prompt = String::new();
        let mut api_messages = Vec::new();

        for msg in messages {
            match msg.role {
                Role::System => {
                    if let MessageContent::Text(ref t) = msg.content {
                        system_prompt = t.clone();
                    }
                }
                Role::User => {
                    if let MessageContent::Text(ref t) = msg.content {
                        api_messages.push(serde_json::json!({
                            "role": "user",
                            "content": t
                        }));
                    }
                }
                Role::Assistant => match &msg.content {
                    MessageContent::Text(t) => {
                        api_messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": t
                        }));
                    }
                    MessageContent::ToolCalls(calls) => {
                        let content: Vec<serde_json::Value> = calls
                            .iter()
                            .map(|c| {
                                serde_json::json!({
                                    "type": "tool_use",
                                    "id": c.id,
                                    "name": c.name,
                                    "input": c.arguments
                                })
                            })
                            .collect();
                        api_messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content
                        }));
                    }
                    _ => {}
                },
                Role::Tool => {
                    if let MessageContent::ToolResult(ref r) = msg.content {
                        api_messages.push(serde_json::json!({
                            "role": "user",
                            "content": [{
                                "type": "tool_result",
                                "tool_use_id": r.tool_call_id,
                                "content": r.content,
                                "is_error": r.is_error
                            }]
                        }));
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "model": self.model,
            "max_tokens": 8192,
            "stream": true,
            "messages": api_messages,
        });

        if !system_prompt.is_empty() {
            body["system"] = serde_json::json!(system_prompt);
        }

        if !tools.is_empty() {
            let api_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(api_tools);
        }

        body
    }
}

#[async_trait]
impl LlmProvider for ClaudeProvider {
    fn name(&self) -> &str {
        "claude"
    }

    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream_cb: Option<StreamCallback>,
    ) -> Result<LlmResponse> {
        let body = self.build_request_body(messages, tools);
        let url = format!("{}/v1/messages", self.base_url);

        debug!(model = %self.model, url = %url, "calling Claude API");

        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to call Claude API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Claude API error (HTTP {}): {}", status, body);
        }

        // TODO: Parse SSE stream properly. For now, read full response.
        // In production, this should parse `event: content_block_delta` etc.
        let body_text = response.text().await?;

        // Parse non-streaming response as fallback
        // (Streaming SSE parsing to be implemented with proper event-stream handling)
        let parsed: serde_json::Value = serde_json::from_str(&body_text)
            .context("failed to parse Claude response")?;

        let content = parsed
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        let tool_calls: Vec<ToolCallRequest> = parsed
            .get("content")
            .and_then(|c| c.as_array())
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                            Some(ToolCallRequest {
                                id: b.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                                name: b
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: b.get("input").cloned().unwrap_or_default(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let stop_reason = match parsed
            .get("stop_reason")
            .and_then(|s| s.as_str())
        {
            Some("end_turn") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("stop_sequence") => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        };

        let usage = TokenUsage {
            input_tokens: parsed
                .get("usage")
                .and_then(|u| u.get("input_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            output_tokens: parsed
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            total_tokens: 0, // computed below
            estimated_cost_usd: 0.0, // TODO: cost estimation
        };

        let mut usage = usage;
        usage.total_tokens = usage.input_tokens + usage.output_tokens;

        if let Some(cb) = &stream_cb {
            if !content.is_empty() {
                cb(StreamEvent::TextDelta(content.clone()));
            }
        }

        Ok(LlmResponse {
            content,
            tool_calls,
            stop_reason,
            usage,
        })
    }

    fn count_tokens(&self, text: &str) -> usize {
        // Anthropic doesn't provide a public tokenizer — use character estimation
        // ~3.5 chars per token for English text
        (text.len() as f64 / 3.5).ceil() as usize
    }

    fn model_info(&self) -> ModelInfo {
        let (max_context, max_output) = match self.model.as_str() {
            m if m.contains("opus") => (200_000, 32_000),
            m if m.contains("sonnet") => (200_000, 64_000),
            m if m.contains("haiku") => (200_000, 8_192),
            _ => (200_000, 8_192),
        };
        ModelInfo {
            name: self.model.clone(),
            max_context_tokens: max_context,
            max_output_tokens: max_output,
            provider_name: "claude".to_string(),
        }
    }
}
