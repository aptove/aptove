//! Context Window Manager
//!
//! Tracks messages and token usage, enforces per-model limits, and supports
//! user-facing commands: display, clear, compact.

use serde::Serialize;

use crate::plugin::{Message, MessageContent, PluginHost, Role};

// ---------------------------------------------------------------------------
// Context display info
// ---------------------------------------------------------------------------

/// Breakdown of a single message's token contribution.
#[derive(Debug, Clone, Serialize)]
pub struct MessageTokenInfo {
    pub index: usize,
    pub role: Role,
    pub tokens: usize,
    /// First N chars of the content for display.
    pub preview: String,
}

/// Full context window status for display.
#[derive(Debug, Clone, Serialize)]
pub struct ContextDisplay {
    pub message_count: usize,
    pub total_tokens: usize,
    pub max_tokens: usize,
    pub usage_percent: f64,
    pub messages: Vec<MessageTokenInfo>,
}

// ---------------------------------------------------------------------------
// Token estimator
// ---------------------------------------------------------------------------

/// Token estimation strategy.
pub type TokenCounter = Box<dyn Fn(&str) -> usize + Send + Sync>;

/// Default token estimator: chars / 4.
pub fn default_token_counter(text: &str) -> usize {
    (text.len() + 3) / 4 // ceiling division
}

// ---------------------------------------------------------------------------
// Context Window
// ---------------------------------------------------------------------------

/// Manages the conversation context for a session.
pub struct ContextWindow {
    /// All messages in the context.
    messages: Vec<Message>,
    /// Estimated tokens per message (parallel to `messages`).
    token_counts: Vec<usize>,
    /// Running total of estimated tokens.
    total_tokens: usize,
    /// Maximum token limit for the active model.
    max_tokens: usize,
    /// Token counting function (can be overridden by provider).
    counter: TokenCounter,
    /// Compaction threshold as fraction of max_tokens (default 0.80).
    compact_target: f64,
}

impl std::fmt::Debug for ContextWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContextWindow")
            .field("messages", &self.messages.len())
            .field("total_tokens", &self.total_tokens)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}

impl ContextWindow {
    /// Create a new context window with the given token limit.
    pub fn new(max_tokens: usize) -> Self {
        Self {
            messages: Vec::new(),
            token_counts: Vec::new(),
            total_tokens: 0,
            max_tokens,
            counter: Box::new(default_token_counter),
            compact_target: 0.80,
        }
    }

    /// Override the token counting function (e.g. with a provider-specific tokenizer).
    pub fn set_token_counter(&mut self, counter: TokenCounter) {
        self.counter = counter;
        // Recount all existing messages
        self.total_tokens = 0;
        for (i, msg) in self.messages.iter().enumerate() {
            let tokens = self.count_message(msg);
            self.token_counts[i] = tokens;
            self.total_tokens += tokens;
        }
    }

    /// Update the maximum token limit (e.g. after model switch).
    pub fn set_max_tokens(&mut self, max_tokens: usize) {
        self.max_tokens = max_tokens;
    }

    /// Add a message to the context. Auto-compacts if over limit.
    pub fn add_message(&mut self, message: Message) {
        let tokens = self.count_message(&message);
        self.messages.push(message);
        self.token_counts.push(tokens);
        self.total_tokens += tokens;

        // Auto-compact if over limit
        if self.total_tokens > self.max_tokens {
            tracing::info!(
                tokens = self.total_tokens,
                max = self.max_tokens,
                "context over limit, auto-compacting"
            );
            self.compact();
        }
    }

    /// Clear all messages except system prompts. Returns the number of
    /// messages removed.
    pub fn clear(&mut self) -> usize {
        let before = self.messages.len();

        // Keep only system messages
        let mut kept_messages = Vec::new();
        let mut kept_tokens = Vec::new();
        let mut kept_total = 0;

        for (i, msg) in self.messages.iter().enumerate() {
            if msg.role == Role::System {
                kept_messages.push(msg.clone());
                kept_tokens.push(self.token_counts[i]);
                kept_total += self.token_counts[i];
            }
        }

        let removed = before - kept_messages.len();
        self.messages = kept_messages;
        self.token_counts = kept_tokens;
        self.total_tokens = kept_total;

        tracing::info!(removed, remaining = self.messages.len(), "context cleared");
        removed
    }

    /// Compact: remove oldest non-system messages until under `compact_target`
    /// fraction of max tokens.
    pub fn compact(&mut self) -> usize {
        let target = (self.max_tokens as f64 * self.compact_target) as usize;
        let mut removed = 0;

        while self.total_tokens > target && !self.messages.is_empty() {
            // Find the first non-system message
            if let Some(idx) = self.messages.iter().position(|m| m.role != Role::System) {
                let tokens = self.token_counts[idx];
                self.messages.remove(idx);
                self.token_counts.remove(idx);
                self.total_tokens -= tokens;
                removed += 1;
            } else {
                // Only system messages left, can't compact further
                break;
            }
        }

        if removed > 0 {
            tracing::info!(
                removed,
                total_tokens = self.total_tokens,
                target,
                "compacted context"
            );
        }
        removed
    }

    /// Compact with plugin strategy override. If a plugin provides
    /// replacement messages via `on_context_compact`, those are used
    /// instead of the default drop-oldest strategy.
    pub async fn compact_with_plugins(&mut self, plugin_host: &PluginHost) -> usize {
        let target = (self.max_tokens as f64 * self.compact_target) as usize;

        if self.total_tokens <= target {
            return 0;
        }

        // Collect the non-system messages that would be dropped
        let droppable: Vec<Message> = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .cloned()
            .collect();

        // Ask plugins for a replacement
        match plugin_host.run_context_compact(&droppable, target).await {
            Ok(Some(replacement)) => {
                // Plugin provided replacement messages — rebuild context
                // Keep system messages, then insert replacements
                let system_msgs: Vec<_> = self
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| m.role == Role::System)
                    .map(|(i, m)| (m.clone(), self.token_counts[i]))
                    .collect();

                let old_count = self.messages.len();
                self.messages.clear();
                self.token_counts.clear();
                self.total_tokens = 0;

                // Re-add system messages
                for (msg, tokens) in &system_msgs {
                    self.messages.push(msg.clone());
                    self.token_counts.push(*tokens);
                    self.total_tokens += tokens;
                }

                // Add plugin-provided replacements
                for msg in &replacement {
                    let tokens = self.count_message(msg);
                    self.messages.push(msg.clone());
                    self.token_counts.push(tokens);
                    self.total_tokens += tokens;
                }

                let removed = old_count.saturating_sub(self.messages.len());
                tracing::info!(
                    removed,
                    replacement_count = replacement.len(),
                    total_tokens = self.total_tokens,
                    "compacted context via plugin strategy"
                );
                removed
            }
            Ok(None) => {
                // No plugin override — fall back to default
                self.compact()
            }
            Err(e) => {
                tracing::warn!(err = %e, "on_context_compact hook error, falling back to default");
                self.compact()
            }
        }
    }

    /// Check if a message of the given size would fit without exceeding the limit.
    pub fn fits(&self, text: &str) -> bool {
        let estimated = (self.counter)(text);
        self.total_tokens + estimated <= self.max_tokens
    }

    /// Return a structured breakdown of the context window.
    pub fn display(&self) -> ContextDisplay {
        let messages = self
            .messages
            .iter()
            .enumerate()
            .map(|(i, msg)| {
                let preview = match &msg.content {
                    MessageContent::Text(t) => {
                        if t.len() > 80 {
                            format!("{}…", &t[..80])
                        } else {
                            t.clone()
                        }
                    }
                    MessageContent::ToolCalls(calls) => {
                        format!("[{} tool call(s)]", calls.len())
                    }
                    MessageContent::ToolResult(r) => {
                        let preview = if r.content.len() > 60 {
                            format!("{}…", &r.content[..60])
                        } else {
                            r.content.clone()
                        };
                        format!("[tool result: {}]", preview)
                    }
                };
                MessageTokenInfo {
                    index: i,
                    role: msg.role.clone(),
                    tokens: self.token_counts[i],
                    preview,
                }
            })
            .collect();

        ContextDisplay {
            message_count: self.messages.len(),
            total_tokens: self.total_tokens,
            max_tokens: self.max_tokens,
            usage_percent: if self.max_tokens > 0 {
                (self.total_tokens as f64 / self.max_tokens as f64) * 100.0
            } else {
                0.0
            },
            messages,
        }
    }

    /// Get all messages (for passing to the LLM).
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Number of messages in the context.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Current total token count.
    pub fn total_tokens(&self) -> usize {
        self.total_tokens
    }

    /// Maximum token limit.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    // -- internal -----------------------------------------------------------

    fn count_message(&self, msg: &Message) -> usize {
        let text_len = msg.content.char_len();
        // Add overhead for role + framing (~4 tokens)
        (self.counter)(&" ".repeat(text_len)) + 4
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Message, MessageContent, Role};

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
        }
    }

    #[test]
    fn add_and_count() {
        let mut ctx = ContextWindow::new(10000);
        ctx.add_message(text_msg(Role::System, "You are helpful."));
        ctx.add_message(text_msg(Role::User, "Hello!"));

        assert_eq!(ctx.message_count(), 2);
        assert!(ctx.total_tokens() > 0);
    }

    #[test]
    fn clear_keeps_system() {
        let mut ctx = ContextWindow::new(10000);
        ctx.add_message(text_msg(Role::System, "System prompt"));
        ctx.add_message(text_msg(Role::User, "Hello"));
        ctx.add_message(text_msg(Role::Assistant, "Hi there"));

        let removed = ctx.clear();
        assert_eq!(removed, 2);
        assert_eq!(ctx.message_count(), 1);
        assert_eq!(ctx.messages()[0].role, Role::System);
    }

    #[test]
    fn compact_removes_oldest_non_system() {
        // Small limit to trigger compaction
        let mut ctx = ContextWindow::new(100);
        ctx.add_message(text_msg(Role::System, "sys"));
        ctx.add_message(text_msg(Role::User, "a]".repeat(50).as_str()));
        ctx.add_message(text_msg(Role::User, "b".repeat(50).as_str()));

        // Should have auto-compacted
        assert!(ctx.total_tokens() <= 100);
    }

    #[test]
    fn fits_check() {
        let mut ctx = ContextWindow::new(100);
        ctx.add_message(text_msg(Role::User, &"x".repeat(300)));

        // After compaction, should report fits for small message
        assert!(ctx.fits("hello"));
    }

    #[test]
    fn display_format() {
        let mut ctx = ContextWindow::new(10000);
        ctx.add_message(text_msg(Role::System, "You are a helpful assistant."));
        ctx.add_message(text_msg(Role::User, "What is 2+2?"));

        let display = ctx.display();
        assert_eq!(display.message_count, 2);
        assert!(display.usage_percent < 100.0);
        assert_eq!(display.messages.len(), 2);
    }
}
