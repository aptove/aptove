//! Workspace Management
//!
//! Defines the `WorkspaceStore` trait, workspace data types, and the
//! `WorkspaceManager` component that orchestrates workspace lifecycle.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::bindings::BindingStore;
use crate::persistence::SessionStore;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Full metadata for a workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceMetadata {
    pub uuid: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub provider: Option<String>,
}

/// Lightweight summary for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub uuid: String,
    pub name: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub provider: Option<String>,
}

impl From<WorkspaceMetadata> for WorkspaceSummary {
    fn from(m: WorkspaceMetadata) -> Self {
        Self {
            uuid: m.uuid,
            name: m.name,
            created_at: m.created_at,
            last_accessed: m.last_accessed,
            provider: m.provider,
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceStore trait
// ---------------------------------------------------------------------------

/// Storage backend for workspace lifecycle operations.
#[async_trait]
pub trait WorkspaceStore: Send + Sync {
    /// Create a new workspace with the given UUID.
    async fn create(
        &self,
        uuid: &str,
        name: Option<&str>,
        provider: Option<&str>,
    ) -> Result<WorkspaceMetadata>;

    /// Load workspace metadata by UUID.
    async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata>;

    /// List all workspaces.
    async fn list(&self) -> Result<Vec<WorkspaceSummary>>;

    /// Delete a workspace and all its data.
    async fn delete(&self, uuid: &str) -> Result<()>;

    /// Update the last_accessed timestamp.
    async fn update_accessed(&self, uuid: &str) -> Result<()>;

    /// Garbage-collect stale workspaces. Returns count removed.
    async fn gc(&self, max_age_days: u64) -> Result<u64>;

    /// Load the per-workspace config override as a TOML string.
    /// Returns `None` if no workspace-level config exists.
    async fn load_config(&self, uuid: &str) -> Result<Option<String>>;
}

// ---------------------------------------------------------------------------
// WorkspaceManager
// ---------------------------------------------------------------------------

/// Central component for workspace lifecycle management.
///
/// Depends on `WorkspaceStore`, `SessionStore`, and `BindingStore` trait objects.
pub struct WorkspaceManager {
    workspace_store: Arc<dyn WorkspaceStore>,
    session_store: Arc<dyn SessionStore>,
    binding_store: Arc<dyn BindingStore>,
}

impl WorkspaceManager {
    /// Create a new workspace manager from storage trait objects.
    pub fn new(
        workspace_store: Arc<dyn WorkspaceStore>,
        session_store: Arc<dyn SessionStore>,
        binding_store: Arc<dyn BindingStore>,
    ) -> Self {
        Self {
            workspace_store,
            session_store,
            binding_store,
        }
    }

    /// Create a new workspace with a random UUID.
    pub async fn create(
        &self,
        name: Option<&str>,
        provider: Option<&str>,
    ) -> Result<WorkspaceMetadata> {
        let uuid = Uuid::new_v4().to_string();
        self.workspace_store.create(&uuid, name, provider).await
    }

    /// Load an existing workspace by UUID.
    pub async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata> {
        self.workspace_store.update_accessed(uuid).await?;
        self.workspace_store.load(uuid).await
    }

    /// Resolve a device ID to a workspace. Creates a new workspace
    /// and binds the device if no binding exists.
    pub async fn resolve(&self, device_id: &str) -> Result<WorkspaceMetadata> {
        if let Some(workspace_id) = self.binding_store.resolve(device_id).await? {
            match self.load(&workspace_id).await {
                Ok(ws) => return Ok(ws),
                Err(_) => {
                    tracing::warn!(
                        device_id,
                        workspace_id = %workspace_id,
                        "bound workspace missing, creating new"
                    );
                }
            }
        }

        // Create new workspace and bind device
        let ws = self.create(None, None).await?;
        self.binding_store.bind(device_id, &ws.uuid).await?;
        tracing::info!(device_id, workspace = %ws.uuid, "created workspace for device");
        Ok(ws)
    }

    /// List all workspaces.
    pub async fn list(&self) -> Result<Vec<WorkspaceSummary>> {
        self.workspace_store.list().await
    }

    /// Delete a workspace, its session, and any device bindings.
    pub async fn delete(&self, uuid: &str) -> Result<()> {
        self.binding_store.unbind_workspace(uuid).await?;
        self.session_store.clear(uuid).await.ok();
        self.workspace_store.delete(uuid).await
    }

    /// Load or create the default workspace (for CLI single-user mode).
    pub async fn default(&self) -> Result<WorkspaceMetadata> {
        match self.workspace_store.load("default").await {
            Ok(ws) => {
                self.workspace_store.update_accessed("default").await?;
                Ok(ws)
            }
            Err(_) => {
                self.workspace_store
                    .create("default", Some("default"), None)
                    .await
            }
        }
    }

    /// Garbage-collect stale workspaces.
    pub async fn gc(&self, max_age_days: u64) -> Result<u64> {
        self.workspace_store.gc(max_age_days).await
    }

    /// Load the workspace-specific config override as a TOML string.
    /// Returns `None` if no per-workspace config exists.
    pub async fn load_workspace_config(&self, uuid: &str) -> Result<Option<String>> {
        self.workspace_store.load_config(uuid).await
    }

    /// Access the workspace store.
    pub fn workspace_store(&self) -> &Arc<dyn WorkspaceStore> {
        &self.workspace_store
    }

    /// Access the session store.
    pub fn session_store(&self) -> &Arc<dyn SessionStore> {
        &self.session_store
    }

    /// Access the binding store.
    pub fn binding_store(&self) -> &Arc<dyn BindingStore> {
        &self.binding_store
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::BindingStore;
    use crate::persistence::{SessionData, SessionStore};
    use crate::types::Message;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    // -- Mock stores -------------------------------------------------------

    struct MockWorkspaceStore {
        workspaces: RwLock<HashMap<String, WorkspaceMetadata>>,
    }

    impl MockWorkspaceStore {
        fn new() -> Self {
            Self {
                workspaces: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl WorkspaceStore for MockWorkspaceStore {
        async fn create(
            &self,
            uuid: &str,
            name: Option<&str>,
            provider: Option<&str>,
        ) -> Result<WorkspaceMetadata> {
            let meta = WorkspaceMetadata {
                uuid: uuid.to_string(),
                name: name.map(|s| s.to_string()),
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                provider: provider.map(|s| s.to_string()),
            };
            self.workspaces
                .write()
                .await
                .insert(uuid.to_string(), meta.clone());
            Ok(meta)
        }

        async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata> {
            self.workspaces
                .read()
                .await
                .get(uuid)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("workspace not found: {}", uuid))
        }

        async fn list(&self) -> Result<Vec<WorkspaceSummary>> {
            Ok(self
                .workspaces
                .read()
                .await
                .values()
                .cloned()
                .map(WorkspaceSummary::from)
                .collect())
        }

        async fn delete(&self, uuid: &str) -> Result<()> {
            self.workspaces.write().await.remove(uuid);
            Ok(())
        }

        async fn update_accessed(&self, uuid: &str) -> Result<()> {
            if let Some(ws) = self.workspaces.write().await.get_mut(uuid) {
                ws.last_accessed = Utc::now();
            }
            Ok(())
        }

        async fn gc(&self, _max_age_days: u64) -> Result<u64> {
            Ok(0)
        }

        async fn load_config(&self, _uuid: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    struct MockSessionStore;

    #[async_trait]
    impl SessionStore for MockSessionStore {
        async fn read(&self, _workspace_id: &str) -> Result<SessionData> {
            Ok(SessionData::new("mock", "mock-model"))
        }
        async fn append(&self, _workspace_id: &str, _message: &Message) -> Result<()> {
            Ok(())
        }
        async fn write(&self, _workspace_id: &str, _data: &SessionData) -> Result<()> {
            Ok(())
        }
        async fn clear(&self, _workspace_id: &str) -> Result<()> {
            Ok(())
        }
    }

    struct MockBindingStore {
        bindings: RwLock<HashMap<String, String>>,
    }

    impl MockBindingStore {
        fn new() -> Self {
            Self {
                bindings: RwLock::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl BindingStore for MockBindingStore {
        async fn resolve(&self, device_id: &str) -> Result<Option<String>> {
            Ok(self.bindings.read().await.get(device_id).cloned())
        }
        async fn bind(&self, device_id: &str, workspace_id: &str) -> Result<()> {
            self.bindings
                .write()
                .await
                .insert(device_id.to_string(), workspace_id.to_string());
            Ok(())
        }
        async fn unbind(&self, device_id: &str) -> Result<()> {
            self.bindings.write().await.remove(device_id);
            Ok(())
        }
        async fn unbind_workspace(&self, workspace_id: &str) -> Result<()> {
            self.bindings
                .write()
                .await
                .retain(|_, v| v != workspace_id);
            Ok(())
        }
    }

    fn make_manager() -> WorkspaceManager {
        WorkspaceManager::new(
            Arc::new(MockWorkspaceStore::new()),
            Arc::new(MockSessionStore),
            Arc::new(MockBindingStore::new()),
        )
    }

    #[tokio::test]
    async fn create_workspace() {
        let mgr = make_manager();
        let ws = mgr.create(Some("test"), None).await.unwrap();
        assert_eq!(ws.name, Some("test".to_string()));
        assert!(!ws.uuid.is_empty());
    }

    #[tokio::test]
    async fn resolve_creates_and_binds() {
        let mgr = make_manager();
        let ws = mgr.resolve("device-1").await.unwrap();
        assert!(!ws.uuid.is_empty());

        // Second resolve returns same workspace
        let ws2 = mgr.resolve("device-1").await.unwrap();
        assert_eq!(ws.uuid, ws2.uuid);
    }

    #[tokio::test]
    async fn resolve_different_devices_get_different_workspaces() {
        let mgr = make_manager();
        let ws1 = mgr.resolve("device-1").await.unwrap();
        let ws2 = mgr.resolve("device-2").await.unwrap();
        assert_ne!(ws1.uuid, ws2.uuid);
    }

    #[tokio::test]
    async fn default_workspace() {
        let mgr = make_manager();
        let ws = mgr.default().await.unwrap();
        assert_eq!(ws.uuid, "default");
        assert_eq!(ws.name, Some("default".to_string()));

        let ws2 = mgr.default().await.unwrap();
        assert_eq!(ws.uuid, ws2.uuid);
    }

    #[tokio::test]
    async fn delete_workspace() {
        let mgr = make_manager();
        let ws = mgr.resolve("device-1").await.unwrap();
        mgr.delete(&ws.uuid).await.unwrap();
        assert!(mgr.workspace_store().load(&ws.uuid).await.is_err());
    }

    #[tokio::test]
    async fn list_workspaces() {
        let mgr = make_manager();
        mgr.create(Some("ws1"), None).await.unwrap();
        mgr.create(Some("ws2"), None).await.unwrap();
        let list = mgr.list().await.unwrap();
        assert_eq!(list.len(), 2);
    }
}
