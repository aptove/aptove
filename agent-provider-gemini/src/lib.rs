//! Google Gemini LLM Provider
//!
//! Implements `LlmProvider` for the Google Gemini API.
//! Supports streaming, function calling, and token counting.

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

/// Google Gemini LLM provider.
pub struct GeminiProvider {
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl GeminiProvider {
    pub fn new(api_key: &str, model: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Convert internal messages to Gemini API format.
    fn build_request_body(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> serde_json::Value {
        let mut contents = Vec::new();
        let mut system_instruction = None;

        for msg in messages {
            match msg.role {
                Role::System => {
                    if let MessageContent::Text(ref t) = msg.content {
                        system_instruction = Some(serde_json::json!({
                            "parts": [{ "text": t }]
                        }));
                    }
                }
                Role::User => {
                    if let MessageContent::Text(ref t) = msg.content {
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": [{ "text": t }]
                        }));
                    }
                }
                Role::Assistant => {
                    if let MessageContent::Text(ref t) = msg.content {
                        contents.push(serde_json::json!({
                            "role": "model",
                            "parts": [{ "text": t }]
                        }));
                    }
                }
                Role::Tool => {
                    if let MessageContent::ToolResult(ref r) = msg.content {
                        contents.push(serde_json::json!({
                            "role": "user",
                            "parts": [{
                                "functionResponse": {
                                    "name": r.tool_call_id,
                                    "response": { "result": r.content }
                                }
                            }]
                        }));
                    }
                }
            }
        }

        let mut body = serde_json::json!({
            "contents": contents,
            "generationConfig": {
                "maxOutputTokens": 8192,
            }
        });

        if let Some(sys) = system_instruction {
            body["systemInstruction"] = sys;
        }

        if !tools.is_empty() {
            let declarations: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters
                    })
                })
                .collect();
            body["tools"] = serde_json::json!([{
                "functionDeclarations": declarations
            }]);
        }

        body
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        stream_cb: Option<StreamCallback>,
    ) -> Result<LlmResponse> {
        let body = self.build_request_body(messages, tools);
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.model, self.api_key
        );

        debug!(model = %self.model, "calling Gemini API");

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to call Gemini API")?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error (HTTP {}): {}", status, body);
        }

        let parsed: serde_json::Value = response.json().await?;

        // Extract text and function calls from candidates
        let mut content = String::new();
        let mut tool_calls = Vec::new();

        if let Some(candidates) = parsed.get("candidates").and_then(|c| c.as_array()) {
            if let Some(candidate) = candidates.first() {
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                            content.push_str(text);
                        }
                        if let Some(fc) = part.get("functionCall") {
                            tool_calls.push(ToolCallRequest {
                                id: uuid::Uuid::new_v4().to_string(),
                                name: fc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: fc.get("args").cloned().unwrap_or_default(),
                            });
                        }
                    }
                }
            }
        }

        let stop_reason = if !tool_calls.is_empty() {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };

        // Gemini usage metadata
        let usage_meta = parsed.get("usageMetadata");
        let usage = TokenUsage {
            input_tokens: usage_meta
                .and_then(|u| u.get("promptTokenCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            output_tokens: usage_meta
                .and_then(|u| u.get("candidatesTokenCount"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize,
            total_tokens: usage_meta
                .and_then(|u| u.get("totalTokenCount"))
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
        // Gemini uses a similar tokenization to SentencePiece
        // ~4 chars per token for English
        (text.len() + 3) / 4
    }

    fn model_info(&self) -> ModelInfo {
        let (max_context, max_output) = match self.model.as_str() {
            m if m.contains("2.5-pro") => (1_048_576, 65_536),
            m if m.contains("2.5-flash") => (1_048_576, 65_536),
            m if m.contains("2.0") => (1_048_576, 8_192),
            _ => (1_048_576, 8_192),
        };
        ModelInfo {
            name: self.model.clone(),
            max_context_tokens: max_context,
            max_output_tokens: max_output,
            provider_name: "gemini".to_string(),
        }
    }
}
