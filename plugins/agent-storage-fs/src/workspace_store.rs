//! Filesystem implementation of `WorkspaceStore`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use agent_core::workspace::{WorkspaceMetadata, WorkspaceStore, WorkspaceSummary};

// ---------------------------------------------------------------------------
// Metadata on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct WorkspaceToml {
    uuid: String,
    #[serde(default)]
    name: Option<String>,
    created_at: String,
    last_accessed: String,
    #[serde(default)]
    provider: Option<String>,
}

impl WorkspaceToml {
    fn to_metadata(&self) -> Result<WorkspaceMetadata> {
        Ok(WorkspaceMetadata {
            uuid: self.uuid.clone(),
            name: self.name.clone(),
            created_at: chrono::DateTime::parse_from_rfc3339(&self.created_at)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            last_accessed: chrono::DateTime::parse_from_rfc3339(&self.last_accessed)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now()),
            provider: self.provider.clone(),
        })
    }

    fn from_metadata(meta: &WorkspaceMetadata) -> Self {
        Self {
            uuid: meta.uuid.clone(),
            name: meta.name.clone(),
            created_at: meta.created_at.to_rfc3339(),
            last_accessed: meta.last_accessed.to_rfc3339(),
            provider: meta.provider.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// FsWorkspaceStore
// ---------------------------------------------------------------------------

/// Filesystem-backed workspace store.
///
/// Workspaces are UUID-named folders under `<data_dir>/workspaces/`.
pub struct FsWorkspaceStore {
    data_dir: PathBuf,
}

impl FsWorkspaceStore {
    /// Create a new store rooted at `data_dir`.
    /// Creates the `workspaces/` subdirectory if needed.
    pub fn new(data_dir: &Path) -> Result<Self> {
        let ws_dir = data_dir.join("workspaces");
        std::fs::create_dir_all(&ws_dir)
            .with_context(|| format!("failed to create workspaces dir: {}", ws_dir.display()))?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
        })
    }

    fn workspaces_dir(&self) -> PathBuf {
        self.data_dir.join("workspaces")
    }

    fn workspace_dir(&self, uuid: &str) -> PathBuf {
        self.workspaces_dir().join(uuid)
    }

    fn metadata_path(&self, uuid: &str) -> PathBuf {
        self.workspace_dir(uuid).join("workspace.toml")
    }

    fn read_metadata(&self, uuid: &str) -> Result<WorkspaceMetadata> {
        let path = self.metadata_path(uuid);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read workspace metadata: {}", path.display()))?;
        let ws_toml: WorkspaceToml = toml::from_str(&content)
            .with_context(|| format!("failed to parse workspace metadata: {}", path.display()))?;
        ws_toml.to_metadata()
    }

    fn write_metadata(&self, meta: &WorkspaceMetadata) -> Result<()> {
        let ws_toml = WorkspaceToml::from_metadata(meta);
        let content = toml::to_string_pretty(&ws_toml)?;
        let path = self.metadata_path(&meta.uuid);
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write workspace metadata: {}", path.display()))?;
        Ok(())
    }
}

#[async_trait]
impl WorkspaceStore for FsWorkspaceStore {
    async fn create(
        &self,
        uuid: &str,
        name: Option<&str>,
        provider: Option<&str>,
    ) -> Result<WorkspaceMetadata> {
        let ws_dir = self.workspace_dir(uuid);
        tokio::fs::create_dir_all(&ws_dir)
            .await
            .with_context(|| format!("failed to create workspace dir: {}", ws_dir.display()))?;

        // Create subdirectories
        for sub in &["mcp", "skills", "memory"] {
            tokio::fs::create_dir_all(ws_dir.join(sub)).await?;
        }

        // Create empty session.md
        let session_path = ws_dir.join("session.md");
        if !session_path.exists() {
            tokio::fs::write(&session_path, "").await?;
        }

        let now = Utc::now();
        let meta = WorkspaceMetadata {
            uuid: uuid.to_string(),
            name: name.map(|s| s.to_string()),
            created_at: now,
            last_accessed: now,
            provider: provider.map(|s| s.to_string()),
        };

        self.write_metadata(&meta)?;

        info!(uuid, name = ?name, "created workspace");
        Ok(meta)
    }

    async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata> {
        self.read_metadata(uuid)
    }

    async fn list(&self) -> Result<Vec<WorkspaceSummary>> {
        let ws_dir = self.workspaces_dir();
        let mut summaries = Vec::new();

        let mut entries = tokio::fs::read_dir(&ws_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let uuid = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();

            match self.read_metadata(&uuid) {
                Ok(meta) => summaries.push(WorkspaceSummary::from(meta)),
                Err(e) => {
                    debug!(uuid = %uuid, err = %e, "skipping corrupt workspace");
                }
            }
        }

        summaries.sort_by(|a, b| b.last_accessed.cmp(&a.last_accessed));
        Ok(summaries)
    }

    async fn delete(&self, uuid: &str) -> Result<()> {
        let ws_dir = self.workspace_dir(uuid);
        if ws_dir.exists() {
            tokio::fs::remove_dir_all(&ws_dir)
                .await
                .with_context(|| format!("failed to delete workspace: {}", ws_dir.display()))?;
            info!(uuid, "deleted workspace");
        }
        Ok(())
    }

    async fn update_accessed(&self, uuid: &str) -> Result<()> {
        match self.read_metadata(uuid) {
            Ok(mut meta) => {
                meta.last_accessed = Utc::now();
                self.write_metadata(&meta)?;
            }
            Err(_) => {} // Workspace doesn't exist, ignore
        }
        Ok(())
    }

    async fn load_config(&self, uuid: &str) -> Result<Option<String>> {
        let config_path = self.workspace_dir(uuid).join("config.toml");
        if config_path.exists() {
            let content = tokio::fs::read_to_string(&config_path)
                .await
                .with_context(|| format!("failed to read workspace config: {}", config_path.display()))?;
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    async fn gc(&self, max_age_days: u64) -> Result<u64> {
        let cutoff = Utc::now() - Duration::days(max_age_days as i64);
        let mut removed = 0u64;

        let summaries = self.list().await?;
        for ws in summaries {
            if ws.uuid == "default" {
                continue; // Never GC the default workspace
            }
            if ws.last_accessed < cutoff {
                self.delete(&ws.uuid).await?;
                removed += 1;
            }
        }

        if removed > 0 {
            info!(removed, max_age_days, "garbage-collected stale workspaces");
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();

        let ws = store.create("test-uuid", Some("My Workspace"), Some("claude")).await.unwrap();
        assert_eq!(ws.uuid, "test-uuid");
        assert_eq!(ws.name, Some("My Workspace".to_string()));
        assert_eq!(ws.provider, Some("claude".to_string()));

        let loaded = store.load("test-uuid").await.unwrap();
        assert_eq!(loaded.uuid, "test-uuid");
        assert_eq!(loaded.name, Some("My Workspace".to_string()));
    }

    #[tokio::test]
    async fn create_makes_subdirs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();

        store.create("test-uuid", None, None).await.unwrap();

        let ws_dir = dir.path().join("workspaces").join("test-uuid");
        assert!(ws_dir.join("mcp").is_dir());
        assert!(ws_dir.join("skills").is_dir());
        assert!(ws_dir.join("memory").is_dir());
        assert!(ws_dir.join("session.md").exists());
    }

    #[tokio::test]
    async fn list_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();

        store.create("ws-1", Some("First"), None).await.unwrap();
        store.create("ws-2", Some("Second"), None).await.unwrap();

        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn delete_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();

        store.create("to-delete", None, None).await.unwrap();
        store.delete("to-delete").await.unwrap();
        assert!(store.load("to-delete").await.is_err());
    }

    #[tokio::test]
    async fn load_missing_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();
        assert!(store.load("nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn gc_removes_old_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsWorkspaceStore::new(dir.path()).unwrap();

        // Create workspace with old timestamp
        store.create("old-ws", None, None).await.unwrap();
        {
            let mut meta = store.read_metadata("old-ws").unwrap();
            meta.last_accessed = Utc::now() - Duration::days(100);
            store.write_metadata(&meta).unwrap();
        }

        store.create("new-ws", None, None).await.unwrap();

        let removed = store.gc(30).await.unwrap();
        assert_eq!(removed, 1);
        assert!(store.load("old-ws").await.is_err());
        assert!(store.load("new-ws").await.is_ok());
    }
}
