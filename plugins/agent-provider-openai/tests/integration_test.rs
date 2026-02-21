use agent_core::provider::{LlmProvider, StopReason};
use agent_core::types::{Message, MessageContent, Role, ToolDefinition};
use agent_provider_openai::OpenAiProvider;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.to_string()),
    }
}

fn openai_text_body(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": text
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    })
}

fn openai_tool_body(tool_id: &str, tool_name: &str, args_json: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": args_json
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {
            "prompt_tokens": 15,
            "completion_tokens": 8,
            "total_tokens": 23
        }
    })
}

#[tokio::test]
async fn test_text_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_text_body("Hello from OpenAI!")))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini", Some(&server.uri()));
    let result = provider.complete(&[user_msg("Hi")], &[], None).await.unwrap();

    assert_eq!(result.content, "Hello from OpenAI!");
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.usage.input_tokens, 10);
    assert_eq!(result.usage.output_tokens, 5);
    assert_eq!(result.usage.total_tokens, 15);
}

#[tokio::test]
async fn test_tool_call_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(openai_tool_body(
                "call_01",
                "bash",
                r#"{"query":"test"}"#,
            )),
        )
        .mount(&server)
        .await;

    let tools = vec![ToolDefinition {
        name: "bash".to_string(),
        description: "Run bash".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];

    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini", Some(&server.uri()));
    let result = provider
        .complete(&[user_msg("run ls")], &tools, None)
        .await
        .unwrap();

    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].id, "call_01");
    assert_eq!(result.tool_calls[0].name, "bash");
    assert_eq!(
        result.tool_calls[0].arguments,
        serde_json::json!({"query": "test"})
    );
    assert_eq!(result.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn test_http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string(
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("bad-key", "gpt-4o-mini", Some(&server.uri()));
    let result = provider.complete(&[user_msg("Hi")], &[], None).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("401"), "expected 401 in error: {err}");
}

#[tokio::test]
async fn test_stream_callback_receives_delta() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_text_body("streamed")))
        .mount(&server)
        .await;

    use std::sync::{Arc, Mutex};
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let clone = collected.clone();

    let cb: agent_core::provider::StreamCallback = std::sync::Arc::new(move |event| {
        if let agent_core::provider::StreamEvent::TextDelta(t) = event {
            clone.lock().unwrap().push(t);
        }
    });

    let provider = OpenAiProvider::new("test-key", "gpt-4o-mini", Some(&server.uri()));
    let result = provider
        .complete(&[user_msg("Hi")], &[], Some(cb))
        .await
        .unwrap();

    assert_eq!(result.content, "streamed");
    let deltas = collected.lock().unwrap();
    assert!(!deltas.is_empty());
}
