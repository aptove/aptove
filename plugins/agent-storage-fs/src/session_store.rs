//! Filesystem implementation of `SessionStore`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::debug;

use agent_core::persistence::{
    parse_session, serialize_message, serialize_session, SessionData, SessionStore,
};
use agent_core::types::Message;

// ---------------------------------------------------------------------------
// FsSessionStore
// ---------------------------------------------------------------------------

/// Filesystem-backed session store.
///
/// Sessions are stored as `session.md` files within workspace directories.
pub struct FsSessionStore {
    data_dir: PathBuf,
}

impl FsSessionStore {
    /// Create a new store rooted at `data_dir`.
    pub fn new(data_dir: &Path) -> Result<Self> {
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
        })
    }

    fn session_path(&self, workspace_id: &str) -> PathBuf {
        self.data_dir
            .join("workspaces")
            .join(workspace_id)
            .join("session.md")
    }
}

#[async_trait]
impl SessionStore for FsSessionStore {
    async fn read(&self, workspace_id: &str) -> Result<SessionData> {
        let path = self.session_path(workspace_id);
        if !path.exists() {
            return Ok(SessionData::new("unknown", "unknown"));
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read session: {}", path.display()))?;

        if content.trim().is_empty() {
            return Ok(SessionData::new("unknown", "unknown"));
        }

        parse_session(&content)
    }

    async fn append(&self, workspace_id: &str, message: &Message) -> Result<()> {
        // Only persist text messages (user, assistant, system)
        if let Some(serialized) = serialize_message(message) {
            let path = self.session_path(workspace_id);

            // If the file doesn't exist or is empty, write a full session
            // with just this message (including frontmatter)
            if !path.exists() {
                let data = SessionData {
                    frontmatter: agent_core::persistence::SessionFrontmatter {
                        provider: "unknown".to_string(),
                        model: "unknown".to_string(),
                        created_at: chrono::Utc::now(),
                    },
                    messages: vec![message.clone()],
                };
                let content = serialize_session(&data);
                tokio::fs::write(&path, content).await?;
                return Ok(());
            }

            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .await
                .with_context(|| format!("failed to open session for append: {}", path.display()))?;

            file.write_all(serialized.as_bytes()).await?;
            file.flush().await?;

            debug!(workspace_id, "appended message to session.md");
        }
        Ok(())
    }

    async fn write(&self, workspace_id: &str, data: &SessionData) -> Result<()> {
        let path = self.session_path(workspace_id);
        let content = serialize_session(data);
        tokio::fs::write(&path, content)
            .await
            .with_context(|| format!("failed to write session: {}", path.display()))?;
        debug!(workspace_id, "wrote session.md");
        Ok(())
    }

    async fn clear(&self, workspace_id: &str) -> Result<()> {
        let path = self.session_path(workspace_id);
        if path.exists() {
            tokio::fs::write(&path, "")
                .await
                .with_context(|| format!("failed to clear session: {}", path.display()))?;
            debug!(workspace_id, "cleared session.md");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::types::{MessageContent, Role, ToolCallResult};

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: MessageContent::Text(text.to_string()),
        }
    }

    async fn setup_store() -> (tempfile::TempDir, FsSessionStore) {
        let dir = tempfile::tempdir().unwrap();
        // Create workspace directory structure
        let ws_dir = dir.path().join("workspaces").join("test-ws");
        tokio::fs::create_dir_all(&ws_dir).await.unwrap();
        tokio::fs::write(ws_dir.join("session.md"), "").await.unwrap();
        let store = FsSessionStore::new(dir.path()).unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn read_empty_session() {
        let (_dir, store) = setup_store().await;
        let data = store.read("test-ws").await.unwrap();
        assert!(data.messages.is_empty());
    }

    #[tokio::test]
    async fn write_and_read() {
        let (_dir, store) = setup_store().await;

        let mut data = SessionData::new("claude", "sonnet");
        data.messages.push(text_msg(Role::User, "Hello"));
        data.messages.push(text_msg(Role::Assistant, "Hi there!"));

        store.write("test-ws", &data).await.unwrap();

        let loaded = store.read("test-ws").await.unwrap();
        assert_eq!(loaded.frontmatter.provider, "claude");
        assert_eq!(loaded.messages.len(), 2);
    }

    #[tokio::test]
    async fn append_messages() {
        let (_dir, store) = setup_store().await;

        // Write initial session
        let data = SessionData::new("claude", "sonnet");
        store.write("test-ws", &data).await.unwrap();

        // Append messages
        store
            .append("test-ws", &text_msg(Role::User, "First message"))
            .await
            .unwrap();
        store
            .append("test-ws", &text_msg(Role::Assistant, "First reply"))
            .await
            .unwrap();

        let loaded = store.read("test-ws").await.unwrap();
        assert_eq!(loaded.messages.len(), 2);
    }

    #[tokio::test]
    async fn append_skips_tool_messages() {
        let (_dir, store) = setup_store().await;

        let data = SessionData::new("claude", "sonnet");
        store.write("test-ws", &data).await.unwrap();

        // Append a text message
        store
            .append("test-ws", &text_msg(Role::User, "Hello"))
            .await
            .unwrap();

        // Try to append a tool result (should be ignored)
        store
            .append(
                "test-ws",
                &Message {
                    role: Role::Tool,
                    content: MessageContent::ToolResult(ToolCallResult {
                        tool_call_id: "tc1".into(),
                        content: "result".into(),
                        is_error: false,
                    }),
                },
            )
            .await
            .unwrap();

        let loaded = store.read("test-ws").await.unwrap();
        assert_eq!(loaded.messages.len(), 1); // Only the text message
    }

    #[tokio::test]
    async fn clear_session() {
        let (_dir, store) = setup_store().await;

        let mut data = SessionData::new("claude", "sonnet");
        data.messages.push(text_msg(Role::User, "Hello"));
        store.write("test-ws", &data).await.unwrap();

        store.clear("test-ws").await.unwrap();

        let loaded = store.read("test-ws").await.unwrap();
        assert!(loaded.messages.is_empty());
    }

    #[tokio::test]
    async fn read_missing_workspace() {
        let (_dir, store) = setup_store().await;
        let data = store.read("nonexistent").await.unwrap();
        assert!(data.messages.is_empty());
    }
}
