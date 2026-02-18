use agent_core::plugin::{LlmProvider, Message, MessageContent, Role, StopReason, ToolDefinition};
use agent_provider_gemini::GeminiProvider;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.to_string()),
    }
}

fn gemini_text_body(text: &str) -> serde_json::Value {
    serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{ "text": text }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 8,
            "candidatesTokenCount": 4,
            "totalTokenCount": 12
        }
    })
}

fn gemini_tool_body(tool_name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "name": tool_name,
                        "args": args
                    }
                }]
            },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 12,
            "candidatesTokenCount": 6,
            "totalTokenCount": 18
        }
    })
}

#[tokio::test]
async fn test_text_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:generateContent.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_body("Hello from Gemini!")))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_base_url("test-key", "gemini-1.5-flash", &server.uri());
    let result = provider.complete(&[user_msg("Hi")], &[], None).await.unwrap();

    assert_eq!(result.content, "Hello from Gemini!");
    assert!(result.tool_calls.is_empty());
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.usage.input_tokens, 8);
    assert_eq!(result.usage.output_tokens, 4);
    assert_eq!(result.usage.total_tokens, 12);
}

#[tokio::test]
async fn test_tool_call_response() {
    let server = MockServer::start().await;

    let args = serde_json::json!({"query": "test"});
    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:generateContent.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(gemini_tool_body("bash", args.clone())),
        )
        .mount(&server)
        .await;

    let tools = vec![ToolDefinition {
        name: "bash".to_string(),
        description: "Run bash".to_string(),
        parameters: serde_json::json!({"type": "object", "properties": {}}),
    }];

    let provider = GeminiProvider::with_base_url("test-key", "gemini-1.5-flash", &server.uri());
    let result = provider
        .complete(&[user_msg("run ls")], &tools, None)
        .await
        .unwrap();

    assert_eq!(result.tool_calls.len(), 1);
    assert_eq!(result.tool_calls[0].name, "bash");
    assert_eq!(result.tool_calls[0].arguments, args);
    assert_eq!(result.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn test_http_error_returns_err() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:generateContent.*"))
        .respond_with(ResponseTemplate::new(403).set_body_string(
            r#"{"error":{"code":403,"message":"API key not valid","status":"PERMISSION_DENIED"}}"#,
        ))
        .mount(&server)
        .await;

    let provider = GeminiProvider::with_base_url("bad-key", "gemini-1.5-flash", &server.uri());
    let result = provider.complete(&[user_msg("Hi")], &[], None).await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("403"), "expected 403 in error: {err}");
}

#[tokio::test]
async fn test_stream_callback_receives_delta() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.*:generateContent.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(gemini_text_body("streamed")))
        .mount(&server)
        .await;

    use std::sync::{Arc, Mutex};
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let clone = collected.clone();

    let cb: agent_core::plugin::StreamCallback = std::sync::Arc::new(move |event| {
        if let agent_core::plugin::StreamEvent::TextDelta(t) = event {
            clone.lock().unwrap().push(t);
        }
    });

    let provider = GeminiProvider::with_base_url("test-key", "gemini-1.5-flash", &server.uri());
    let result = provider
        .complete(&[user_msg("Hi")], &[], Some(cb))
        .await
        .unwrap();

    assert_eq!(result.content, "streamed");
    let deltas = collected.lock().unwrap();
    assert!(!deltas.is_empty());
}
