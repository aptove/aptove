//! `agent-bridge` — Embedded bridge server for `aptove serve`.
//!
//! Wires an [`AgentRuntime`] directly to a [`StdioBridge`] via in-process
//! channels, so the agent and bridge server run in the same process with no
//! subprocess spawning or stdio piping.

pub mod config;
pub mod server;

pub use config::BridgeServeConfig;
pub use server::BridgeServer;
