//! Device-to-Workspace Bindings
//!
//! Defines the `BindingStore` trait for mapping device identifiers
//! to workspace UUIDs. Implementations live in storage backend crates.

use anyhow::Result;
use async_trait::async_trait;

// ---------------------------------------------------------------------------
// BindingStore trait
// ---------------------------------------------------------------------------

/// Storage backend for device → workspace bindings.
#[async_trait]
pub trait BindingStore: Send + Sync {
    /// Resolve a device ID to a workspace UUID.
    /// Returns `None` if no binding exists.
    async fn resolve(&self, device_id: &str) -> Result<Option<String>>;

    /// Bind a device to a workspace. Overwrites any existing binding.
    async fn bind(&self, device_id: &str, workspace_id: &str) -> Result<()>;

    /// Remove the binding for a device.
    async fn unbind(&self, device_id: &str) -> Result<()>;

    /// Remove all bindings that point to a given workspace.
    async fn unbind_workspace(&self, workspace_id: &str) -> Result<()>;
}
