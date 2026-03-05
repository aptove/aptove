//! `agent-tui` — Full-screen terminal UI for Aptove.
//!
//! Built on [Ratatui](https://ratatui.rs) with Crossterm as the terminal backend.
//!
//! # Usage
//!
//! ```no_run
//! use agent_core::config::AgentConfig;
//! use agent_core::builder::AgentRuntime;
//!
//! async fn launch(config: AgentConfig, runtime: AgentRuntime) -> anyhow::Result<()> {
//!     agent_tui::run(config, runtime).await
//! }
//! ```

pub mod app;
pub mod tabs;
pub mod services;
pub mod widgets;

use anyhow::Result;
use agent_core::config::AgentConfig;
use agent_core::builder::AgentRuntime;

/// Launch the full-screen TUI.
///
/// This call blocks until the user quits. On exit the terminal is restored
/// to its prior state regardless of whether the function returns `Ok` or `Err`.
pub async fn run(config: AgentConfig, runtime: AgentRuntime) -> Result<()> {
    app::run_app(config, runtime).await
}
