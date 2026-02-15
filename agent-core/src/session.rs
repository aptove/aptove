//! Session Management
//!
//! Manages ACP sessions: creation, prompt handling, cancellation.
//! Each session owns a [`ContextWindow`] and tracks metadata.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::context::ContextWindow;
use crate::plugin::{Message, Role, MessageContent};

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A unique session identifier.
pub type SessionId = String;

/// Session mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Coding,
    Planning,
    Reviewing,
}

impl Default for SessionMode {
    fn default() -> Self {
        Self::Coding
    }
}

/// A single agent session.
#[derive(Debug)]
pub struct Session {
    /// Unique session id.
    pub id: SessionId,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// Current mode.
    pub mode: SessionMode,
    /// Context window for this session.
    pub context: ContextWindow,
    /// Cancellation token for the current in-flight prompt.
    pub cancel_token: CancellationToken,
    /// Provider name used for this session.
    pub provider: String,
    /// Model name used for this session.
    pub model: String,
    /// Whether a prompt is currently being processed.
    pub is_busy: bool,
}

impl Session {
    /// Create a new session with the given context limits.
    pub fn new(max_tokens: usize, provider: &str, model: &str) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            mode: SessionMode::default(),
            context: ContextWindow::new(max_tokens),
            cancel_token: CancellationToken::new(),
            provider: provider.to_string(),
            model: model.to_string(),
            is_busy: false,
        }
    }

    /// Add a user message to the session context.
    pub fn add_user_message(&mut self, text: &str) {
        self.context.add_message(Message {
            role: Role::User,
            content: MessageContent::Text(text.to_string()),
        });
    }

    /// Add an assistant message to the session context.
    pub fn add_assistant_message(&mut self, text: &str) {
        self.context.add_message(Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.to_string()),
        });
    }

    /// Cancel the current in-flight operation.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Reset the cancellation token for a new prompt.
    pub fn reset_cancel(&mut self) {
        self.cancel_token = CancellationToken::new();
    }
}

// ---------------------------------------------------------------------------
// Session summary (for persistence / listing)
// ---------------------------------------------------------------------------

/// Lightweight summary of a session for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub created_at: DateTime<Utc>,
    pub message_count: usize,
    pub provider: String,
    pub model: String,
    pub mode: SessionMode,
}

// ---------------------------------------------------------------------------
// Session Manager
// ---------------------------------------------------------------------------

/// Manages multiple sessions.
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<SessionId, Arc<Mutex<Session>>>>>,
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a new session and return its id.
    pub async fn create_session(
        &self,
        max_tokens: usize,
        provider: &str,
        model: &str,
    ) -> SessionId {
        let session = Session::new(max_tokens, provider, model);
        let id = session.id.clone();
        tracing::info!(session_id = %id, "created session");

        let mut sessions = self.sessions.write().await;
        sessions.insert(id.clone(), Arc::new(Mutex::new(session)));
        id
    }

    /// Get a session by id.
    pub async fn get_session(&self, id: &str) -> Result<Arc<Mutex<Session>>> {
        let sessions = self.sessions.read().await;
        sessions
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", id))
    }

    /// Cancel a session's in-flight operation.
    pub async fn cancel_session(&self, id: &str) -> Result<()> {
        let session_arc = self.get_session(id).await?;
        let session = session_arc.lock().await;
        session.cancel();
        tracing::info!(session_id = %id, "cancelled session");
        Ok(())
    }

    /// Remove a session.
    pub async fn remove_session(&self, id: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;
        if sessions.remove(id).is_some() {
            tracing::info!(session_id = %id, "removed session");
            Ok(())
        } else {
            bail!("session '{}' not found", id)
        }
    }

    /// List all session summaries.
    pub async fn list_sessions(&self) -> Vec<SessionSummary> {
        let sessions = self.sessions.read().await;
        let mut summaries = Vec::new();
        for (_, session_arc) in sessions.iter() {
            let session = session_arc.lock().await;
            summaries.push(SessionSummary {
                id: session.id.clone(),
                created_at: session.created_at,
                message_count: session.context.message_count(),
                provider: session.provider.clone(),
                model: session.model.clone(),
                mode: session.mode.clone(),
            });
        }
        summaries
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_get_session() {
        let mgr = SessionManager::new();
        let id = mgr.create_session(4096, "claude", "claude-sonnet-4-20250514").await;

        let session_arc = mgr.get_session(&id).await.unwrap();
        let session = session_arc.lock().await;
        assert_eq!(session.provider, "claude");
        assert_eq!(session.mode, SessionMode::Coding);
    }

    #[tokio::test]
    async fn cancel_session() {
        let mgr = SessionManager::new();
        let id = mgr.create_session(4096, "claude", "claude-sonnet-4-20250514").await;

        mgr.cancel_session(&id).await.unwrap();

        let session_arc = mgr.get_session(&id).await.unwrap();
        let session = session_arc.lock().await;
        assert!(session.cancel_token.is_cancelled());
    }

    #[tokio::test]
    async fn list_sessions() {
        let mgr = SessionManager::new();
        mgr.create_session(4096, "claude", "sonnet").await;
        mgr.create_session(8192, "openai", "gpt-4o").await;

        let summaries = mgr.list_sessions().await;
        assert_eq!(summaries.len(), 2);
    }

    #[tokio::test]
    async fn missing_session_error() {
        let mgr = SessionManager::new();
        assert!(mgr.get_session("nonexistent").await.is_err());
    }
}
