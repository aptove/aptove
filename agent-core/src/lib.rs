//! # Aptove Agent Core
//!
//! Core library for the Aptove ACP-compatible AI coding agent.
//! Provides transport, session management, context window, plugin system,
//! and the agentic tool loop.

pub mod agent_loop;
pub mod config;
pub mod context;
pub mod persistence;
pub mod plugin;
pub mod retry;
pub mod session;
pub mod system_prompt;
pub mod transport;

// Re-export key types
pub use config::AgentConfig;
pub use context::ContextWindow;
pub use plugin::{LlmProvider, LlmResponse, ModelInfo, Plugin, PluginHost, StopReason, TokenUsage};
pub use session::{Session, SessionManager};
pub use transport::StdioTransport;
