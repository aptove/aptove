//! Anthropic Claude LLM Provider
//!
//! Implements `LlmProvider` for the Anthropic Messages API.
//! Supports streaming, tool use, and token counting.

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use tracing::{debug, info, warn};

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
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
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

        info!(model = %self.model, url = %url, "calling Claude API (streaming)");

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

        // Parse SSE stream from Anthropic Messages API
        let mut full_text = String::new();
        let mut tool_calls: Vec<ToolCallRequest> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut input_tokens: usize = 0;
        let mut output_tokens: usize = 0;

        // Track in-progress tool call inputs (index → partial JSON string)
        let mut tool_input_buffers: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        // Track tool call metadata (index → (id, name))
        let mut tool_meta: std::collections::HashMap<usize, (String, String)> =
            std::collections::HashMap::new();

        info!("Claude API connected, reading SSE stream");

        let mut stream = response.bytes_stream();
        let mut line_buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("error reading Claude stream chunk")?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buffer.push_str(&chunk_str);

            // Process complete lines from buffer
            while let Some(newline_pos) = line_buffer.find('\n') {
                let line = line_buffer[..newline_pos].trim_end_matches('\r').to_string();
                line_buffer = line_buffer[newline_pos + 1..].to_string();

                // SSE format: lines starting with "data: " contain JSON
                if let Some(data) = line.strip_prefix("data: ") {
                    if data == "[DONE]" {
                        continue;
                    }
                    let event: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("failed to parse SSE data: {} — line: {}", e, data);
                            continue;
                        }
                    };

                    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

                    match event_type {
                        "message_start" => {
                            // Extract input token count from message.usage
                            if let Some(usage) = event
                                .get("message")
                                .and_then(|m| m.get("usage"))
                            {
                                input_tokens = usage
                                    .get("input_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as usize;
                            }
                        }
                        "content_block_start" => {
                            let index = event
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            if let Some(block) = event.get("content_block") {
                                let block_type =
                                    block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                if block_type == "tool_use" {
                                    let id = block
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let name = block
                                        .get("name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    // Emit tool call start via stream callback
                                    if let Some(ref cb) = stream_cb {
                                        cb(StreamEvent::ToolCallStart {
                                            id: id.clone(),
                                            name: name.clone(),
                                        });
                                    }
                                    tool_meta.insert(index, (id, name));
                                    tool_input_buffers.insert(index, String::new());
                                }
                            }
                        }
                        "content_block_delta" => {
                            let index = event
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            if let Some(delta) = event.get("delta") {
                                let delta_type =
                                    delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                                match delta_type {
                                    "text_delta" => {
                                        if let Some(text) =
                                            delta.get("text").and_then(|t| t.as_str())
                                        {
                                            full_text.push_str(text);
                                            if let Some(ref cb) = stream_cb {
                                                cb(StreamEvent::TextDelta(text.to_string()));
                                            }
                                        }
                                    }
                                    "input_json_delta" => {
                                        if let Some(partial) =
                                            delta.get("partial_json").and_then(|t| t.as_str())
                                        {
                                            if let Some(buf) = tool_input_buffers.get_mut(&index) {
                                                buf.push_str(partial);
                                            }
                                            // Emit tool call argument delta
                                            if let Some((ref id, _)) = tool_meta.get(&index) {
                                                if let Some(ref cb) = stream_cb {
                                                    cb(StreamEvent::ToolCallDelta {
                                                        id: id.clone(),
                                                        arguments_delta: partial.to_string(),
                                                    });
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_stop" => {
                            let index = event
                                .get("index")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            // Finalize tool call if this was a tool_use block
                            if let Some((id, name)) = tool_meta.remove(&index) {
                                let input_json = tool_input_buffers.remove(&index).unwrap_or_default();
                                let arguments: serde_json::Value =
                                    serde_json::from_str(&input_json).unwrap_or_default();
                                tool_calls.push(ToolCallRequest {
                                    id,
                                    name,
                                    arguments,
                                });
                            }
                        }
                        "message_delta" => {
                            // Extract stop_reason and output token count
                            if let Some(delta) = event.get("delta") {
                                stop_reason = match delta
                                    .get("stop_reason")
                                    .and_then(|s| s.as_str())
                                {
                                    Some("end_turn") => StopReason::EndTurn,
                                    Some("tool_use") => StopReason::ToolUse,
                                    Some("max_tokens") => StopReason::MaxTokens,
                                    Some("stop_sequence") => StopReason::StopSequence,
                                    _ => StopReason::EndTurn,
                                };
                            }
                            if let Some(usage) = event.get("usage") {
                                output_tokens = usage
                                    .get("output_tokens")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0) as usize;
                            }
                        }
                        "message_stop" | "ping" => {
                            // No action needed
                        }
                        "error" => {
                            let err_msg = event
                                .get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error");
                            anyhow::bail!("Claude streaming error: {}", err_msg);
                        }
                        _ => {
                            debug!(event_type, "unhandled Claude SSE event type");
                        }
                    }
                }
            }
        }

        info!(
            text_len = full_text.len(),
            tool_call_count = tool_calls.len(),
            input_tokens,
            output_tokens,
            "Claude SSE stream complete"
        );

        let usage = TokenUsage {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
            estimated_cost_usd: 0.0,
        };

        Ok(LlmResponse {
            content: full_text,
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
