//! Plugin Trait System
//!
//! Defines the `Plugin` trait for lifecycle hooks and the `OutputChunk` type.

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{Message, ToolCallRequest, ToolCallResult};

/// A chunk of output flowing to the user. Plugins can inspect or mutate it
/// via the `on_output_chunk` hook.
#[derive(Debug, Clone)]
pub struct OutputChunk {
    /// The text content of the chunk.
    pub text: String,
    /// Whether this chunk is the final one in the current assistant turn.
    pub is_final: bool,
}

// ---------------------------------------------------------------------------
// Plugin trait (lifecycle)
// ---------------------------------------------------------------------------

/// General plugin trait for lifecycle hooks.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// Unique plugin name.
    fn name(&self) -> &str;

    /// Called once during agent startup. Receives the plugin's config section.
    async fn on_init(&mut self, config: &serde_json::Value) -> Result<()> {
        let _ = config;
        Ok(())
    }

    /// Called during graceful shutdown.
    async fn on_shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Called before a tool call is dispatched. Plugins can inspect or
    /// mutate the request (e.g. inject arguments, block calls).
    async fn on_before_tool_call(&self, _call: &mut ToolCallRequest) -> Result<()> {
        Ok(())
    }

    /// Called after a tool call completes. Plugins can inspect the request
    /// and mutate the result (e.g. post-process output, record undo state).
    async fn on_after_tool_call(
        &self,
        _call: &ToolCallRequest,
        _result: &mut ToolCallResult,
    ) -> Result<()> {
        Ok(())
    }

    /// Called for each chunk of output flowing to the user. Plugins can
    /// mutate the chunk (e.g. apply formatting, syntax highlighting).
    async fn on_output_chunk(&self, _chunk: &mut OutputChunk) -> Result<()> {
        Ok(())
    }

    /// Called when the context window is about to compact. If a plugin
    /// returns `Some(messages)` those replace the dropped messages (e.g.
    /// with a summary). The first plugin to return `Some` wins.
    async fn on_context_compact(
        &self,
        _messages: &[Message],
        _target_tokens: usize,
    ) -> Result<Option<Vec<Message>>> {
        Ok(None)
    }

    /// Called when a new session starts. Plugins can inject initial
    /// context, warn about state, etc.
    async fn on_session_start(&self, _session: &mut crate::session::Session) -> Result<()> {
        Ok(())
    }
}
