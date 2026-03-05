//! Configuration for the embedded bridge server.
//!
//! Transport settings, identity (agent_id, auth_token), and TLS configuration
//! are now owned by [`AgentConfig`] in `config.toml`. This struct holds only
//! the remaining bridge-specific serve settings (bind_addr, advertise_addr,
//! keep_alive).

use serde::{Deserialize, Serialize};

/// Bridge serve configuration — contains only settings NOT owned by AgentConfig.
///
/// Populated from `AgentConfig.serve` in `agent-cli`. Transport selection,
/// identity, and TLS config are read directly from `AgentConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeServeConfig {
    /// Bind address (default `0.0.0.0`).
    pub bind_addr: String,
    /// Enable keep-alive agent pool.
    pub keep_alive: bool,
    /// Override the IP/hostname advertised in the QR code pairing URL.
    /// Useful when running inside Docker or Apple Native containers where
    /// `local_ip_address::local_ip()` returns an internal virtual IP.
    /// Set this to the host machine's real LAN IP (e.g. "192.168.1.50").
    /// Only affects local transport; Cloudflare and Tailscale are unaffected.
    pub advertise_addr: Option<String>,
}

impl Default for BridgeServeConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0".to_string(),
            keep_alive: false,
            advertise_addr: None,
        }
    }
}
