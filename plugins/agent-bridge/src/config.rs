//! Configuration for the embedded bridge server.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unified configuration for `aptove serve` — merges agent config fields
/// with bridge transport/TLS/auth settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeServeConfig {
    /// TCP port for the WebSocket server.
    pub port: u16,
    /// Bind address (default `0.0.0.0`).
    pub bind_addr: String,
    /// Optional bearer token required for WebSocket connections.
    pub auth_token: Option<String>,
    /// Enable TLS (self-signed cert generated on first run).
    pub tls: bool,
    /// Enable keep-alive agent pool.
    pub keep_alive: bool,
    /// Transport mode.
    pub transport: ServeTransport,
    /// Directory for bridge config files (certs, auth token).
    /// Defaults to `~/.config/aptove/bridge/`.
    pub config_dir: PathBuf,
}

/// Transport mode for `aptove serve`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServeTransport {
    /// Local network with self-signed TLS certificate pinning.
    #[default]
    Local,
    /// Cloudflare Zero Trust tunnel.
    Cloudflare,
    /// Tailscale via `tailscale serve` (HTTPS, no cert pinning).
    TailscaleServe,
    /// Tailscale direct IP with self-signed TLS + cert pinning.
    TailscaleIp,
}

impl Default for BridgeServeConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            bind_addr: "0.0.0.0".to_string(),
            auth_token: None,
            tls: true,
            keep_alive: false,
            transport: ServeTransport::default(),
            config_dir: default_config_dir(),
        }
    }
}

impl BridgeServeConfig {
    /// Load config from `config_dir/bridge.toml`, falling back to defaults.
    pub fn load() -> Result<Self> {
        let path = default_config_dir().join("bridge.toml");
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            let cfg: BridgeServeConfig = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse bridge.toml: {}", e))?;
            Ok(cfg)
        } else {
            Ok(Self::default())
        }
    }

    /// Persist config to `config_dir/bridge.toml`.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let path = self.config_dir.join("bridge.toml");
        let contents = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {}", e))?;
        std::fs::write(path, contents)?;
        Ok(())
    }
}

fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("aptove")
        .join("bridge")
}
