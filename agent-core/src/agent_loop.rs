//! Agentic Tool Loop
//!
//! Core loop: prompt → LLM → if tool calls, execute tools → feed results
//! back → repeat until `end_turn` or max iterations.

use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use agent_client_protocol_schema::{
    ContentBlock, ContentChunk, SessionNotification, SessionUpdate, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};

use crate::plugin::{
    LlmProvider, Message, MessageContent, Role, StopReason, StreamCallback,
    StreamEvent, ToolCallRequest, ToolCallResult, ToolDefinition, TokenUsage,
};
use crate::transport::TransportSender;

// Note: Provider is passed as Arc<dyn LlmProvider> — no RwLock needed
// since the provider for a given execution is fixed.

// ---------------------------------------------------------------------------
// Tool executor callback
// ---------------------------------------------------------------------------

/// Callback that executes a tool call and returns the result.
/// Provided by the MCP bridge or other tool source.
pub type ToolExecutor =
    Arc<dyn Fn(ToolCallRequest) -> futures::future::BoxFuture<'static, ToolCallResult> + Send + Sync>;

// ---------------------------------------------------------------------------
// Agent loop
// ---------------------------------------------------------------------------

/// Configuration for the agent loop.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    /// Maximum tool-call iterations before forced stop (default 25).
    pub max_iterations: usize,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            max_iterations: 25,
        }
    }
}

/// Result of running the agent loop for a single prompt.
#[derive(Debug)]
pub struct AgentLoopResult {
    /// All messages generated during the loop (assistant + tool results).
    pub new_messages: Vec<Message>,
    /// Accumulated token usage across all LLM calls.
    pub total_usage: TokenUsage,
    /// Final stop reason.
    pub stop_reason: StopReason,
    /// Number of iterations performed.
    pub iterations: usize,
    /// Whether the loop was cancelled.
    pub cancelled: bool,
}

/// Run the agentic tool loop.
///
/// Takes the conversation messages so far, calls the LLM, executes any
/// tool calls, feeds results back, and repeats until the LLM signals
/// completion or the iteration limit is reached.
pub async fn run_agent_loop(
    provider: Arc<dyn LlmProvider>,
    messages: &[Message],
    tools: &[ToolDefinition],
    tool_executor: Option<ToolExecutor>,
    config: &AgentLoopConfig,
    cancel_token: CancellationToken,
    transport: Option<TransportSender>,
    session_id: &str,
) -> Result<AgentLoopResult> {
    let mut all_messages: Vec<Message> = messages.to_vec();
    let mut new_messages: Vec<Message> = Vec::new();
    let mut total_usage = TokenUsage::default();
    let mut iterations = 0;

    loop {
        // Check cancellation
        if cancel_token.is_cancelled() {
            info!(session_id, "agent loop cancelled");
            return Ok(AgentLoopResult {
                new_messages,
                total_usage,
                stop_reason: StopReason::Error,
                iterations,
                cancelled: true,
            });
        }

        // Check iteration limit
        if iterations >= config.max_iterations {
            warn!(
                session_id,
                iterations,
                max = config.max_iterations,
                "agent loop hit iteration limit"
            );
            return Ok(AgentLoopResult {
                new_messages,
                total_usage,
                stop_reason: StopReason::MaxTokens,
                iterations,
                cancelled: false,
            });
        }

        iterations += 1;
        debug!(session_id, iteration = iterations, "agent loop iteration");

        // Build streaming callback that forwards to transport
        let stream_cb: Option<StreamCallback> = transport.as_ref().map(|tx| {
            let tx = tx.clone();
            let sid = session_id.to_string();
            Arc::new(move |event: StreamEvent| {
                let tx = tx.clone();
                let sid = sid.clone();
                tokio::spawn(async move {
                    let _ = emit_stream_event(&tx, &sid, &event).await;
                });
            }) as StreamCallback
        });

        // Call the LLM
        let response = provider.complete(&all_messages, tools, stream_cb).await?;

        // Accumulate usage
        total_usage.input_tokens += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;
        total_usage.total_tokens += response.usage.total_tokens;
        total_usage.estimated_cost_usd += response.usage.estimated_cost_usd;

        // Handle text content
        if !response.content.is_empty() {
            let msg = Message {
                role: Role::Assistant,
                content: MessageContent::Text(response.content.clone()),
            };
            all_messages.push(msg.clone());
            new_messages.push(msg);
        }

        // Handle tool calls
        if response.tool_calls.is_empty() {
            // No tool calls — we're done
            debug!(session_id, "LLM returned end_turn (no tool calls)");
            return Ok(AgentLoopResult {
                new_messages,
                total_usage,
                stop_reason: response.stop_reason,
                iterations,
                cancelled: false,
            });
        }

        // Add tool call message
        let tool_call_msg = Message {
            role: Role::Assistant,
            content: MessageContent::ToolCalls(response.tool_calls.clone()),
        };
        all_messages.push(tool_call_msg.clone());
        new_messages.push(tool_call_msg);

        // Execute each tool call
        for call in &response.tool_calls {
            debug!(
                session_id,
                tool = %call.name,
                id = %call.id,
                "executing tool call"
            );

            // Emit tool_call_start notification
            if let Some(ref tx) = transport {
                let notification = SessionNotification::new(
                    session_id.to_string(),
                    SessionUpdate::ToolCall(
                        ToolCall::new(call.id.to_string(), call.name.to_string())
                            .status(ToolCallStatus::InProgress),
                    ),
                );
                let _ = tx
                    .send_notification(
                        "session/update",
                        serde_json::to_value(&notification).unwrap_or_default(),
                    )
                    .await;
            }

            let result = if let Some(ref executor) = tool_executor {
                executor(call.clone()).await
            } else {
                // No tool executor available
                ToolCallResult {
                    tool_call_id: call.id.clone(),
                    content: "Error: no tool executor configured".to_string(),
                    is_error: true,
                }
            };

            // Emit tool_call_update notification with result
            if let Some(ref tx) = transport {
                let status = if result.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                };
                let fields = ToolCallUpdateFields::new()
                    .status(status)
                    .content(vec![agent_client_protocol_schema::ToolCallContent::Content(
                        agent_client_protocol_schema::Content::new(
                            ContentBlock::Text(TextContent::new(&result.content)),
                        ),
                    )]);
                let notification = SessionNotification::new(
                    session_id.to_string(),
                    SessionUpdate::ToolCallUpdate(
                        ToolCallUpdate::new(result.tool_call_id.clone(), fields),
                    ),
                );
                let _ = tx
                    .send_notification(
                        "session/update",
                        serde_json::to_value(&notification).unwrap_or_default(),
                    )
                    .await;
            }

            // Add tool result to conversation
            let result_msg = Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(result),
            };
            all_messages.push(result_msg.clone());
            new_messages.push(result_msg);
        }

        // If stop reason is EndTurn despite having tool calls, respect it
        if response.stop_reason == StopReason::EndTurn {
            return Ok(AgentLoopResult {
                new_messages,
                total_usage,
                stop_reason: StopReason::EndTurn,
                iterations,
                cancelled: false,
            });
        }

        // Otherwise, loop back to re-prompt the LLM with tool results
    }
}

/// Emit a stream event as a session/update notification.
async fn emit_stream_event(
    tx: &TransportSender,
    session_id: &str,
    event: &StreamEvent,
) -> Result<()> {
    let notification = match event {
        StreamEvent::TextDelta(text) => SessionNotification::new(
            session_id.to_string(),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(text),
            ))),
        ),
        StreamEvent::ToolCallStart { id, name } => SessionNotification::new(
            session_id.to_string(),
            SessionUpdate::ToolCall(
                ToolCall::new(id.to_string(), name.to_string()).status(ToolCallStatus::InProgress),
            ),
        ),
        StreamEvent::ToolCallDelta { id, arguments_delta } => {
            let fields = ToolCallUpdateFields::new()
                .raw_input(serde_json::json!({ "argumentsDelta": arguments_delta }));
            SessionNotification::new(
                session_id.to_string(),
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id.to_string(), fields)),
            )
        }
    };

    tx.send_notification(
        "session/update",
        serde_json::to_value(&notification).unwrap_or_default(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::*;
    use async_trait::async_trait;

    struct MockProvider {
        responses: std::sync::Mutex<Vec<LlmResponse>>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn complete(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
            _stream_cb: Option<StreamCallback>,
        ) -> Result<LlmResponse> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(LlmResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::default(),
                })
            } else {
                Ok(responses.remove(0))
            }
        }
        fn model_info(&self) -> ModelInfo {
            ModelInfo {
                name: "mock".into(),
                max_context_tokens: 4096,
                max_output_tokens: 1024,
                provider_name: "mock".into(),
            }
        }
    }

    #[tokio::test]
    async fn single_turn_no_tools() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            responses: std::sync::Mutex::new(vec![LlmResponse {
                content: "Hello!".into(),
                tool_calls: vec![],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    estimated_cost_usd: 0.0,
                },
            }]),
        });

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text("Hi".into()),
        }];

        let result = run_agent_loop(
            provider,
            &messages,
            &[],
            None,
            &AgentLoopConfig::default(),
            CancellationToken::new(),
            None,
            "test-session",
        )
        .await
        .unwrap();

        assert_eq!(result.iterations, 1);
        assert_eq!(result.stop_reason, StopReason::EndTurn);
        assert!(!result.cancelled);
        assert_eq!(result.total_usage.input_tokens, 10);
    }

    #[tokio::test]
    async fn max_iterations_limit() {
        // Provider always returns tool calls
        // (The MockProvider below is overridden by AlwaysToolsProvider)

        // Override to always return tool calls
        struct AlwaysToolsProvider;
        #[async_trait]
        impl LlmProvider for AlwaysToolsProvider {
            fn name(&self) -> &str { "always-tools" }
            async fn complete(
                &self, _: &[Message], _: &[ToolDefinition], _: Option<StreamCallback>,
            ) -> Result<LlmResponse> {
                Ok(LlmResponse {
                    content: String::new(),
                    tool_calls: vec![ToolCallRequest {
                        id: "tc1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"cmd": "ls"}),
                    }],
                    stop_reason: StopReason::ToolUse,
                    usage: TokenUsage::default(),
                })
            }
            fn model_info(&self) -> ModelInfo {
                ModelInfo {
                    name: "mock".into(),
                    max_context_tokens: 4096,
                    max_output_tokens: 1024,
                    provider_name: "mock".into(),
                }
            }
        }

        let provider: Arc<dyn LlmProvider> = Arc::new(AlwaysToolsProvider);

        let config = AgentLoopConfig { max_iterations: 3 };

        let result = run_agent_loop(
            provider,
            &[],
            &[],
            None,
            &config,
            CancellationToken::new(),
            None,
            "test",
        )
        .await
        .unwrap();

        assert_eq!(result.iterations, 3);
        assert!(!result.cancelled);
    }
}
