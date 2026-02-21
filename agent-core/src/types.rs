//! Core Data Types
//!
//! Shared message, role, and tool types used across the agent codebase.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Message types
// ---------------------------------------------------------------------------

/// A message in the conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

/// Message role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Message content — text, tool calls, or tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    ToolCalls(Vec<ToolCallRequest>),
    ToolResult(ToolCallResult),
}

impl MessageContent {
    /// Rough character count for token estimation.
    pub fn char_len(&self) -> usize {
        match self {
            MessageContent::Text(t) => t.len(),
            MessageContent::ToolCalls(calls) => {
                calls.iter().map(|c| c.name.len() + c.arguments.to_string().len()).sum()
            }
            MessageContent::ToolResult(r) => r.content.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool types
// ---------------------------------------------------------------------------

/// A tool call requested by the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    /// Unique tool-call id assigned by the provider.
    pub id: String,
    /// Name of the tool to invoke.
    pub name: String,
    /// JSON arguments to pass to the tool.
    pub arguments: serde_json::Value,
}

/// Result returned from executing a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    /// The tool call id this result corresponds to.
    pub tool_call_id: String,
    /// The tool's output content.
    pub content: String,
    /// Whether the tool execution failed.
    pub is_error: bool,
}

/// A tool definition advertised to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
}
