//! Filesystem implementation of `BindingStore`.
//!
//! Device-to-workspace bindings are stored in a single TOML file
//! (`bindings.toml`) at the root of the data directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::debug;

use agent_core::bindings::BindingStore;

// ---------------------------------------------------------------------------
// File format
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct BindingsFile {
    #[serde(default)]
    bindings: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// FsBindingStore
// ---------------------------------------------------------------------------

/// Filesystem-backed binding store.
///
/// Stores device→workspace mappings in `<data_dir>/bindings.toml`.
/// Uses an in-memory `RwLock` cache for fast lookups, flushing to disk
/// on every mutation.
pub struct FsBindingStore {
    path: PathBuf,
    cache: RwLock<BindingsFile>,
}

impl FsBindingStore {
    /// Create a new binding store backed by `<data_dir>/bindings.toml`.
    pub async fn new(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("bindings.toml");
        let file = if path.exists() {
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("failed to read bindings: {}", path.display()))?;
            toml::from_str::<BindingsFile>(&content).unwrap_or_default()
        } else {
            BindingsFile::default()
        };

        Ok(Self {
            path,
            cache: RwLock::new(file),
        })
    }

    /// Create synchronously (for contexts without an async runtime yet).
    pub fn new_sync(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("bindings.toml");
        let file = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read bindings: {}", path.display()))?;
            toml::from_str::<BindingsFile>(&content).unwrap_or_default()
        } else {
            BindingsFile::default()
        };

        Ok(Self {
            path,
            cache: RwLock::new(file),
        })
    }

    /// Flush the in-memory cache to disk.
    async fn flush(&self, data: &BindingsFile) -> Result<()> {
        let content = toml::to_string_pretty(data)
            .context("failed to serialize bindings")?;
        tokio::fs::write(&self.path, content)
            .await
            .with_context(|| format!("failed to write bindings: {}", self.path.display()))?;
        Ok(())
    }
}

#[async_trait]
impl BindingStore for FsBindingStore {
    async fn resolve(&self, device_id: &str) -> Result<Option<String>> {
        let cache = self.cache.read().await;
        Ok(cache.bindings.get(device_id).cloned())
    }

    async fn bind(&self, device_id: &str, workspace_id: &str) -> Result<()> {
        let mut cache = self.cache.write().await;
        cache
            .bindings
            .insert(device_id.to_string(), workspace_id.to_string());
        self.flush(&cache).await?;
        debug!(device_id, workspace_id, "bound device to workspace");
        Ok(())
    }

    async fn unbind(&self, device_id: &str) -> Result<()> {
        let mut cache = self.cache.write().await;
        cache.bindings.remove(device_id);
        self.flush(&cache).await?;
        debug!(device_id, "unbound device");
        Ok(())
    }

    async fn unbind_workspace(&self, workspace_id: &str) -> Result<()> {
        let mut cache = self.cache.write().await;
        cache.bindings.retain(|_, v| v != workspace_id);
        self.flush(&cache).await?;
        debug!(workspace_id, "unbound all devices from workspace");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> (tempfile::TempDir, FsBindingStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBindingStore::new(dir.path()).await.unwrap();
        (dir, store)
    }

    #[tokio::test]
    async fn resolve_returns_none_for_unknown_device() {
        let (_dir, store) = setup_store().await;
        assert!(store.resolve("unknown").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bind_and_resolve() {
        let (_dir, store) = setup_store().await;
        store.bind("dev-1", "ws-abc").await.unwrap();
        assert_eq!(
            store.resolve("dev-1").await.unwrap(),
            Some("ws-abc".to_string())
        );
    }

    #[tokio::test]
    async fn unbind_device() {
        let (_dir, store) = setup_store().await;
        store.bind("dev-1", "ws-abc").await.unwrap();
        store.unbind("dev-1").await.unwrap();
        assert!(store.resolve("dev-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn unbind_workspace_removes_all_device_bindings() {
        let (_dir, store) = setup_store().await;
        store.bind("dev-1", "ws-abc").await.unwrap();
        store.bind("dev-2", "ws-abc").await.unwrap();
        store.bind("dev-3", "ws-other").await.unwrap();

        store.unbind_workspace("ws-abc").await.unwrap();

        assert!(store.resolve("dev-1").await.unwrap().is_none());
        assert!(store.resolve("dev-2").await.unwrap().is_none());
        assert_eq!(
            store.resolve("dev-3").await.unwrap(),
            Some("ws-other".to_string())
        );
    }

    #[tokio::test]
    async fn bindings_persist_to_disk() {
        let dir = tempfile::tempdir().unwrap();

        // Bind in one store instance
        {
            let store = FsBindingStore::new(dir.path()).await.unwrap();
            store.bind("dev-1", "ws-abc").await.unwrap();
        }

        // Reload in a new instance
        {
            let store = FsBindingStore::new(dir.path()).await.unwrap();
            assert_eq!(
                store.resolve("dev-1").await.unwrap(),
                Some("ws-abc".to_string())
            );
        }
    }

    #[tokio::test]
    async fn multiple_bindings() {
        let (_dir, store) = setup_store().await;
        store.bind("dev-1", "ws-1").await.unwrap();
        store.bind("dev-2", "ws-2").await.unwrap();
        store.bind("dev-3", "ws-1").await.unwrap();

        assert_eq!(store.resolve("dev-1").await.unwrap(), Some("ws-1".into()));
        assert_eq!(store.resolve("dev-2").await.unwrap(), Some("ws-2".into()));
        assert_eq!(store.resolve("dev-3").await.unwrap(), Some("ws-1".into()));
    }

    #[tokio::test]
    async fn rebind_device_to_different_workspace() {
        let (_dir, store) = setup_store().await;
        store.bind("dev-1", "ws-old").await.unwrap();
        store.bind("dev-1", "ws-new").await.unwrap();
        assert_eq!(
            store.resolve("dev-1").await.unwrap(),
            Some("ws-new".into())
        );
    }
}
