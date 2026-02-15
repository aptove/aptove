//! Filesystem Storage Backend for Aptove
//!
//! Implements `WorkspaceStore`, `SessionStore`, and `BindingStore` using
//! the local filesystem. Workspace data lives under:
//!
//! ```text
//! <data_dir>/Aptove/
//! ├── config.toml              # Global config
//! ├── bindings.toml             # Device → workspace mapping
//! └── workspaces/
//!     ├── <uuid>/
//!     │   ├── workspace.toml    # Metadata
//!     │   ├── config.toml       # Per-workspace overrides
//!     │   ├── session.md        # Persistent session
//!     │   ├── mcp/              # MCP server logs
//!     │   ├── skills/           # Future
//!     │   └── memory/           # Future
//!     └── default/
//!         └── ...
//! ```

mod binding_store;
mod session_store;
mod workspace_store;

pub use binding_store::FsBindingStore;
pub use session_store::FsSessionStore;
pub use workspace_store::FsWorkspaceStore;
