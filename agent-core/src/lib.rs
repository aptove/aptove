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
pub mod provider;
pub mod retry;
pub mod scheduler;
pub mod session;
pub mod system_prompt;
pub mod transport;
pub mod trigger;
pub mod types;
pub mod services;
pub mod workspace;

// Re-export key types
pub use bindings::BindingStore;
pub use builder::{AgentBuilder, AgentRuntime};
pub use config::AgentConfig;
pub use context::ContextWindow;
pub use persistence::{SessionData, SessionStore};
pub use plugin::{OutputChunk, Plugin};
pub use provider::{LlmProvider, LlmResponse, ModelInfo, StopReason, TokenUsage};
pub use types::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult, ToolDefinition};
pub use scheduler::{JobDefinition, JobRun, JobStatus, JobSummary, SchedulerStore};
pub use session::{Session, SessionManager};
pub use transport::{StdioTransport, InProcessTransport, Transport};
pub use trigger::{
    TriggerDefinition, TriggerEvent, TriggerRun, TriggerStatus, TriggerStore, TriggerSummary,
    generate_token, render_template,
};
pub use workspace::{WorkspaceManager, WorkspaceMetadata, WorkspaceStore, WorkspaceSummary};
pub use services::{
    job_service::JobService,
    session_service::SessionService,
    trigger_service::TriggerService,
};
