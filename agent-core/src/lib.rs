//! # Aptove Agent Core
//!
//! Core library for the Aptove ACP-compatible AI coding agent.
//! Provides transport, session management, context window, plugin system,
//! workspace management, and the agentic tool loop.

pub mod agent_loop;
pub mod bindings;
pub mod builder;
pub mod config;
pub mod context;
pub mod persistence;
pub mod plugin;
pub mod retry;
pub mod scheduler;
pub mod session;
pub mod system_prompt;
pub mod transport;
pub mod workspace;

// Re-export key types
pub use bindings::BindingStore;
pub use builder::{AgentBuilder, AgentRuntime};
pub use config::AgentConfig;
pub use context::ContextWindow;
pub use persistence::{SessionData, SessionStore};
pub use plugin::{LlmProvider, LlmResponse, ModelInfo, OutputChunk, Plugin, PluginHost, StopReason, TokenUsage};
pub use scheduler::{JobDefinition, JobRun, JobStatus, JobSummary, SchedulerStore};
pub use session::{Session, SessionManager};
pub use transport::{StdioTransport, InProcessTransport};
pub use workspace::{WorkspaceManager, WorkspaceMetadata, WorkspaceStore, WorkspaceSummary};
