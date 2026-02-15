//! Session Persistence
//!
//! Workspace-scoped session storage. Each workspace has a single `session.md`
//! file that stores the persistent conversation as structured markdown with
//! YAML-like frontmatter.
//!
//! The `SessionStore` trait defines the operations; filesystem implementation
//! lives in `agent-storage-fs`.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::plugin::{Message, MessageContent, Role};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Metadata stored in the session.md frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFrontmatter {
    pub provider: String,
    pub model: String,
    pub created_at: DateTime<Utc>,
}

/// Complete session data: frontmatter + messages.
#[derive(Debug, Clone)]
pub struct SessionData {
    pub frontmatter: SessionFrontmatter,
    pub messages: Vec<Message>,
}

impl SessionData {
    /// Create an empty session with the given provider and model.
    pub fn new(provider: &str, model: &str) -> Self {
        Self {
            frontmatter: SessionFrontmatter {
                provider: provider.to_string(),
                model: model.to_string(),
                created_at: Utc::now(),
            },
            messages: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SessionStore trait
// ---------------------------------------------------------------------------

/// Workspace-scoped session persistence.
///
/// All operations are keyed by workspace UUID. Each workspace has exactly
/// one session (the persistent conversation).
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Read the full session for a workspace.
    async fn read(&self, workspace_id: &str) -> Result<SessionData>;

    /// Append a single message to the session. Only persists text messages
    /// (user, assistant, system). Tool calls and results are ephemeral.
    async fn append(&self, workspace_id: &str, message: &Message) -> Result<()>;

    /// Write the full session (overwrites existing).
    async fn write(&self, workspace_id: &str, data: &SessionData) -> Result<()>;

    /// Clear the session (reset to empty frontmatter).
    async fn clear(&self, workspace_id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// session.md format: serialization
// ---------------------------------------------------------------------------

/// Serialize a full session to `session.md` format.
pub fn serialize_session(data: &SessionData) -> String {
    let mut out = String::new();

    // Frontmatter
    out.push_str("---\n");
    out.push_str(&format!("provider: {}\n", data.frontmatter.provider));
    out.push_str(&format!("model: {}\n", data.frontmatter.model));
    out.push_str(&format!(
        "created_at: {}\n",
        data.frontmatter.created_at.to_rfc3339()
    ));
    out.push_str("---\n\n");

    // Messages (only text messages are persisted)
    for msg in &data.messages {
        if let Some(s) = serialize_message(msg) {
            out.push_str(&s);
        }
    }

    out
}

/// Serialize a single message. Returns `None` for non-text messages
/// (tool calls, tool results) which are ephemeral.
pub fn serialize_message(msg: &Message) -> Option<String> {
    let role_header = match msg.role {
        Role::System => "## System",
        Role::User => "## User",
        Role::Assistant => "## Assistant",
        Role::Tool => return None, // Tool results are ephemeral
    };

    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            Some(format!("{}\n\n{}\n\n", role_header, t))
        }
        _ => None, // Skip tool calls and empty text
    }
}

// ---------------------------------------------------------------------------
// session.md format: parsing
// ---------------------------------------------------------------------------

/// Parse a `session.md` file into `SessionData`.
pub fn parse_session(content: &str) -> Result<SessionData> {
    let content = content.trim();

    if content.is_empty() {
        return Ok(SessionData::new("unknown", "unknown"));
    }

    // Parse frontmatter
    let (frontmatter, body) = if content.starts_with("---") {
        let after_first = &content[3..];
        if let Some(end) = after_first.find("\n---") {
            let fm_text = after_first[..end].trim();
            let body = after_first[end + 4..].trim();
            (parse_frontmatter(fm_text), body.to_string())
        } else {
            (default_frontmatter(), content.to_string())
        }
    } else {
        (default_frontmatter(), content.to_string())
    };

    let messages = parse_messages(&body);

    Ok(SessionData {
        frontmatter,
        messages,
    })
}

fn default_frontmatter() -> SessionFrontmatter {
    SessionFrontmatter {
        provider: "unknown".to_string(),
        model: "unknown".to_string(),
        created_at: Utc::now(),
    }
}

fn parse_frontmatter(text: &str) -> SessionFrontmatter {
    let mut provider = "unknown".to_string();
    let mut model = "unknown".to_string();
    let mut created_at = Utc::now();

    for line in text.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "provider" => provider = value.to_string(),
                "model" => model = value.to_string(),
                "created_at" => {
                    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
                        created_at = dt.with_timezone(&Utc);
                    }
                }
                _ => {}
            }
        }
    }

    SessionFrontmatter {
        provider,
        model,
        created_at,
    }
}

fn parse_messages(body: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut current_role: Option<Role> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        if let Some(role) = parse_role_header(line) {
            // Save previous message
            if let Some(prev_role) = current_role.take() {
                let text = current_content.trim().to_string();
                if !text.is_empty() {
                    messages.push(Message {
                        role: prev_role,
                        content: MessageContent::Text(text),
                    });
                }
            }
            current_role = Some(role);
            current_content.clear();
        } else {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // Last message
    if let Some(role) = current_role {
        let text = current_content.trim().to_string();
        if !text.is_empty() {
            messages.push(Message {
                role,
                content: MessageContent::Text(text),
            });
        }
    }

    messages
}

fn parse_role_header(line: &str) -> Option<Role> {
    match line.trim() {
        "## System" => Some(Role::System),
        "## User" => Some(Role::User),
        "## Assistant" => Some(Role::Assistant),
        "## Tool" => Some(Role::Tool),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
        }
    }

    #[test]
    fn serialize_empty_session() {
        let data = SessionData::new("claude", "sonnet");
        let md = serialize_session(&data);
        assert!(md.starts_with("---\n"));
        assert!(md.contains("provider: claude"));
        assert!(md.contains("model: sonnet"));
    }

    #[test]
    fn serialize_with_messages() {
        let mut data = SessionData::new("claude", "sonnet");
        data.messages.push(text_msg(Role::User, "Hello!"));
        data.messages.push(text_msg(Role::Assistant, "Hi there."));

        let md = serialize_session(&data);
        assert!(md.contains("## User\n\nHello!"));
        assert!(md.contains("## Assistant\n\nHi there."));
    }

    #[test]
    fn serialize_skips_tool_messages() {
        use crate::plugin::{ToolCallRequest, ToolCallResult};

        let mut data = SessionData::new("claude", "sonnet");
        data.messages.push(text_msg(Role::User, "Run ls"));
        data.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::ToolCalls(vec![ToolCallRequest {
                id: "tc1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({}),
            }]),
        });
        data.messages.push(Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolCallResult {
                tool_call_id: "tc1".into(),
                content: "file1.rs".into(),
                is_error: false,
            }),
        });
        data.messages
            .push(text_msg(Role::Assistant, "Here are the files."));

        let md = serialize_session(&data);
        assert!(md.contains("## User\n\nRun ls"));
        assert!(md.contains("## Assistant\n\nHere are the files."));
        // Tool call and result not serialized
        assert!(!md.contains("bash"));
        assert!(!md.contains("file1.rs"));
    }

    #[test]
    fn parse_round_trip() {
        let mut data = SessionData::new("gemini", "2.5-pro");
        data.messages.push(text_msg(Role::User, "What is 2+2?"));
        data.messages
            .push(text_msg(Role::Assistant, "The answer is 4."));

        let md = serialize_session(&data);
        let parsed = parse_session(&md).unwrap();

        assert_eq!(parsed.frontmatter.provider, "gemini");
        assert_eq!(parsed.frontmatter.model, "2.5-pro");
        assert_eq!(parsed.messages.len(), 2);

        if let MessageContent::Text(ref t) = parsed.messages[0].content {
            assert_eq!(t, "What is 2+2?");
        } else {
            panic!("expected text message");
        }

        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
    }

    #[test]
    fn parse_empty() {
        let parsed = parse_session("").unwrap();
        assert!(parsed.messages.is_empty());
        assert_eq!(parsed.frontmatter.provider, "unknown");
    }

    #[test]
    fn parse_frontmatter_only() {
        let md = "---\nprovider: openai\nmodel: gpt-4o\n---\n";
        let parsed = parse_session(md).unwrap();
        assert_eq!(parsed.frontmatter.provider, "openai");
        assert_eq!(parsed.frontmatter.model, "gpt-4o");
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn parse_multi_turn() {
        let md = r#"---
provider: claude
model: sonnet
created_at: 2026-02-15T10:30:00+00:00
---

## User

Hello, how are you?

## Assistant

I'm doing well, thanks for asking!

## User

Can you help me fix a bug?

## Assistant

Of course! Please share the code.
"#;

        let parsed = parse_session(md).unwrap();
        assert_eq!(parsed.frontmatter.provider, "claude");
        assert_eq!(parsed.messages.len(), 4);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert_eq!(parsed.messages[2].role, Role::User);
        assert_eq!(parsed.messages[3].role, Role::Assistant);
    }

    #[test]
    fn parse_no_frontmatter() {
        let md = "## User\n\nHello!\n\n## Assistant\n\nHi!\n";
        let parsed = parse_session(md).unwrap();
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.frontmatter.provider, "unknown");
    }
}
