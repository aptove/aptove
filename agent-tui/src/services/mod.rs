//! Background service tasks for the TUI.
//!
//! Each service runs as an independent tokio task and broadcasts status
//! updates to `AppState` via `tokio::sync::watch` channels.

pub mod tailscale;
pub mod cloudflare;
pub mod bridge;
