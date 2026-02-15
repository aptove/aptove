//! OpenAI LLM Provider
//!
//! Implements `LlmProvider` for the OpenAI Chat Completions API.
//! Supports streaming, tool use (function calling), and exact token
//! counting via tiktoken (when available).

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

/// OpenAI Chat Completions LLM provider.
/// Also supports OpenAI-compatible endpoints (Together, Groq, local, etc.)
pub struct OpenAiProvider {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    pub fn new(api_key: &str, model: &str, base_url: Option<&str>) -> Self {
        Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            base_url: base_url
                .unwrap_or("https://api.openai.com")
                .to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Convert internal messages to OpenAI API format.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .filter_map(|msg| {
                match msg.role {
                    Role::System => {
                        if let MessageContent::Text(ref t) = msg.content {
                            Some(serde_json::json!({
                                "role": "system",
                                "content": t
                            }))
                        } else {
                            None
                        }
                    }
                    Role::User => {
                        if let MessageContent::Text(ref t) = msg.content {
                            Some(serde_json::json!({
                                "role": "user",
                                "content": t
                            }))
                        } else {
                            None
                        }
                    }
                    Role::Assistant => match &msg.content {
                        MessageContent::Text(t) => Some(serde_json::json!({
                            "role": "assistant",
                            "content": t
                        })),
                        MessageContent::ToolCalls(calls) => {
                            let tc: Vec<serde_json::Value> = calls
                                .iter()
                                .map(|c| {
                                    serde_json::json!({
                                        "id": c.id,
                                        "type": "function",
                                        "function": {
                                            "name": c.name,
                                            "arguments": c.arguments.to_string()
                                        }
                                    })
                                })
                                .collect();
                            Some(serde_json::json!({
                                "role": "assistant",
                                "tool_calls": tc
                            }))
                        }
                        _ => None,
                    },
                    Role::Tool => {
                        if let MessageContent::ToolResult(ref r) = msg.content {
                            Some(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": r.tool_call_id,
                                "content": r.content
                            }))
                        } else {
                            None
                        }
                    }
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": api_messages,
            "stream": false, // Non-streaming for initial impl
        });

        if !tools.is_empty() {
            let api_tools: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(api_tools);
        }

        body
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        "openai"
    }

    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream_cb: Option<StreamCallback>,
    ) -> Result<LlmResponse> {
        let body = self.build_request_body(messages, tools);
        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!(model = %self.model, url = %url, "calling OpenAI API");

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to call OpenAI API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error (HTTP {}): {}", status, body);
        }

        let parsed: serde_json::Value = response.json().await?;

        // Extract from choices[0].message
        let choice = parsed
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"));

        let content = choice
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();

        let tool_calls: Vec<ToolCallRequest> = choice
            .and_then(|m| m.get("tool_calls"))
            .and_then(|tc| tc.as_array())
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|tc| {
                        let func = tc.get("function")?;
                        Some(ToolCallRequest {
                            id: tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            name: func
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string(),
                            arguments: func
                                .get("arguments")
                                .and_then(|a| a.as_str())
                                .and_then(|s| serde_json::from_str(s).ok())
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let finish_reason = parsed
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
            .and_then(|c| c.get("finish_reason"))
            .and_then(|r| r.as_str());

        let stop_reason = match finish_reason {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            _ => {
                if !tool_calls.is_empty() {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }
            }
        };

        let usage = TokenUsage {
            input_tokens: parsed
                .get("usage")
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            output_tokens: parsed
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            total_tokens: parsed
                .get("usage")
                .and_then(|u| u.get("total_tokens"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            estimated_cost_usd: 0.0,
        };

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
        // TODO: Use tiktoken-rs for exact counting
        // For now, ~4 chars per token for English
        (text.len() + 3) / 4
    }

    fn model_info(&self) -> ModelInfo {
        let (max_context, max_output) = match self.model.as_str() {
            "gpt-4o" | "gpt-4o-2024-05-13" => (128_000, 16_384),
            "gpt-4o-mini" => (128_000, 16_384),
            m if m.starts_with("gpt-4-turbo") => (128_000, 4_096),
            "gpt-4" => (8_192, 4_096),
            "o1" | "o1-preview" => (200_000, 100_000),
            "o3-mini" => (200_000, 100_000),
            _ => (128_000, 16_384),
        };
        ModelInfo {
            name: self.model.clone(),
            max_context_tokens: max_context,
            max_output_tokens: max_output,
            provider_name: "openai".to_string(),
        }
    }
}
