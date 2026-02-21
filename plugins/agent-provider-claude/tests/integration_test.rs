use agent_core::provider::{LlmProvider, StopReason};
use agent_core::types::{Message, MessageContent, Role, ToolDefinition};
use agent_provider_claude::ClaudeProvider;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.to_string()),
    }
}

fn sse_text_response(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":10,\"output_tokens\":0}}}}}}\n\n\
event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

fn sse_tool_response(tool_id: &str, tool_name: &str) -> String {
    // args_json is the literal JSON that will be parsed; we emit it as a partial_json delta.
    // The outer SSE data payload is itself JSON so braces must be escaped.
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":15,\"output_tokens\":0}}}}}}\n\n\
event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{tool_id}\",\"name\":\"{tool_name}\"}}}}\n\n\
event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":\"{{\\\"query\\\":\\\"test\\\"}}\"}}}}\n\n\
event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":8}}}}\n\n\
event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

#[tokio::test]
async fn test_text_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_text_response("Hello from Claude!"))
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("test-key", "claude-3-haiku-20240307", Some(&server.uri()));
    let result = provider.complete(&[user_msg("Hi")], &[], None).await.unwrap();

    assert_eq!(result.content, "Hello from Claude!");
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.usage.input_tokens, 10);
    assert_eq!(result.usage.output_tokens, 5);
}

#[tokio::test]
async fn test_tool_call_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_tool_response("toolu_01", "bash"))
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    let tools = vec![ToolDefinition {
        name: "bash".to_string(),
        description: "Run a bash command".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];

    let provider = ClaudeProvider::new("test-key", "claude-3-haiku-20240307", Some(&server.uri()));
    let result = provider
        .complete(&[user_msg("run ls")], &tools, None)
        .await
        .unwrap();

    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].id, "toolu_01");
    assert_eq!(result.tool_calls[0].name, "bash");
    assert_eq!(result.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn test_http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"type":"authentication_error","message":"Invalid API key"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = ClaudeProvider::new("bad-key", "claude-3-haiku-20240307", Some(&server.uri()));
    let result = provider.complete(&[user_msg("Hi")], &[], None).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("401"), "expected 401 in error: {err}");
}

#[tokio::test]
async fn test_stream_callback_receives_deltas() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(sse_text_response("streamed"))
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;

    use std::sync::{Arc, Mutex};
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_clone = collected.clone();

    let cb: agent_core::provider::StreamCallback = std::sync::Arc::new(move |event| {
        if let agent_core::provider::StreamEvent::TextDelta(t) = event {
            collected_clone.lock().unwrap().push(t);
        }
    });

    let provider = ClaudeProvider::new("test-key", "claude-3-haiku-20240307", Some(&server.uri()));
    let result = provider
        .complete(&[user_msg("Hi")], &[], Some(cb))
        .await
        .unwrap();

    assert_eq!(result.content, "streamed");
    let deltas = collected.lock().unwrap();
    assert!(!deltas.is_empty());
    assert_eq!(deltas.join(""), "streamed");
}
