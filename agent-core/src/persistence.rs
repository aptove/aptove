//! Session Persistence
//!
//! Save and load sessions to/from disk as JSON files.
//! Default location: `~/.local/share/Aptove/sessions/`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::plugin::{Message, TokenUsage};
use crate::session::{SessionId, SessionMode, SessionSummary};

// ---------------------------------------------------------------------------
// Serializable session state
// ---------------------------------------------------------------------------

/// Complete session state for persistence.
#[derive(Debug, Serialize, Deserialize)]
pub struct PersistedSession {
    pub id: SessionId,
    pub created_at: DateTime<Utc>,
    pub mode: SessionMode,
    pub provider: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub total_usage: TokenUsage,
}

// ---------------------------------------------------------------------------
// Session store trait
// ---------------------------------------------------------------------------

/// Trait for session persistence backends.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    /// Save a session to the store.
    async fn save(&self, session: &PersistedSession) -> Result<()>;

    /// Load a session by id.
    async fn load(&self, id: &str) -> Result<PersistedSession>;

    /// List all saved session summaries.
    async fn list(&self) -> Result<Vec<SessionSummary>>;

    /// Delete a session by id.
    async fn delete(&self, id: &str) -> Result<()>;
}

// ---------------------------------------------------------------------------
// File-based session store
// ---------------------------------------------------------------------------

/// Stores sessions as JSON files in a directory.
pub struct FileSessionStore {
    dir: PathBuf,
}

impl FileSessionStore {
    /// Create a new file session store.
    /// Creates the directory if it doesn't exist.
    pub fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create session dir: {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
        })
    }

    /// Create a store at the default location:
    /// `~/.local/share/Aptove/sessions/`
    pub fn default_location() -> Result<Self> {
        let base = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine local data directory"))?;
        let dir = base.join("Aptove").join("sessions");
        Self::new(&dir)
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", id))
    }
}

#[async_trait::async_trait]
impl SessionStore for FileSessionStore {
    async fn save(&self, session: &PersistedSession) -> Result<()> {
        let path = self.session_path(&session.id);
        let json = serde_json::to_string_pretty(session)
            .context("failed to serialize session")?;

        tokio::fs::write(&path, json)
            .await
            .with_context(|| format!("failed to write session file: {}", path.display()))?;

        debug!(session_id = %session.id, path = %path.display(), "saved session");
        Ok(())
    }

    async fn load(&self, id: &str) -> Result<PersistedSession> {
        let path = self.session_path(id);
        let json = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read session file: {}", path.display()))?;

        let session: PersistedSession = serde_json::from_str(&json)
            .with_context(|| format!("failed to parse session file: {}", path.display()))?;

        info!(session_id = %id, "loaded session");
        Ok(session)
    }

    async fn list(&self) -> Result<Vec<SessionSummary>> {
        let mut summaries = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match tokio::fs::read_to_string(&path).await {
                Ok(json) => match serde_json::from_str::<PersistedSession>(&json) {
                    Ok(session) => {
                        summaries.push(SessionSummary {
                            id: session.id,
                            created_at: session.created_at,
                            message_count: session.messages.len(),
                            provider: session.provider,
                            model: session.model,
                            mode: session.mode,
                        });
                    }
                    Err(e) => {
                        warn!(path = %path.display(), err = %e, "skipping corrupt session file");
                    }
                },
                Err(e) => {
                    warn!(path = %path.display(), err = %e, "failed to read session file");
                }
            }
        }

        // Sort by creation date, newest first
        summaries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(summaries)
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let path = self.session_path(id);
        tokio::fs::remove_file(&path)
            .await
            .with_context(|| format!("failed to delete session: {}", path.display()))?;
        info!(session_id = %id, "deleted session");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::TokenUsage;

    fn make_test_session(id: &str) -> PersistedSession {
        PersistedSession {
            id: id.to_string(),
            created_at: Utc::now(),
            mode: SessionMode::Coding,
            provider: "claude".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            messages: vec![],
            total_usage: TokenUsage::default(),
        }
    }

    #[tokio::test]
    async fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(dir.path()).unwrap();
        let session = make_test_session("test-1");

        store.save(&session).await.unwrap();
        let loaded = store.load("test-1").await.unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.provider, "claude");
    }

    #[tokio::test]
    async fn list_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(dir.path()).unwrap();

        store.save(&make_test_session("s1")).await.unwrap();
        store.save(&make_test_session("s2")).await.unwrap();

        let summaries = store.list().await.unwrap();
        assert_eq!(summaries.len(), 2);
    }

    #[tokio::test]
    async fn load_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(dir.path()).unwrap();
        assert!(store.load("nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn delete_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileSessionStore::new(dir.path()).unwrap();

        store.save(&make_test_session("to-delete")).await.unwrap();
        store.delete("to-delete").await.unwrap();
        assert!(store.load("to-delete").await.is_err());
    }
}
