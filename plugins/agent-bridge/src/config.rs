//! Configuration for the embedded bridge server.

use anyhow::Result;
use bridge::common_config::CommonConfig;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unified configuration for `aptove run` — merges agent config fields
/// with bridge transport/TLS/auth settings.
///
/// Transport selection is now driven by [`CommonConfig::enabled_transports()`];
/// the `transport` field that previously existed here has been removed.
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
    /// Stable agent identity string (UUID v4), used in pairing responses.
    /// Loaded from `common.toml` in the bridge config directory; generated
    /// automatically on first use.
    pub agent_id: String,
    /// Directory for bridge config files (certs, auth token).
    /// Defaults to `~/.config/aptove/bridge/`.
    pub config_dir: PathBuf,
}

impl Default for BridgeServeConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            bind_addr: "0.0.0.0".to_string(),
            auth_token: None,
            tls: true,
            keep_alive: false,
            agent_id: String::new(),
            config_dir: default_config_dir(),
        }
    }
}

impl BridgeServeConfig {
    /// Load config from `config_dir/bridge.toml`, falling back to defaults.
    ///
    /// Also reads `common.toml` from the same directory to populate `agent_id`
    /// (generating and persisting one if absent).
    pub fn load() -> Result<Self> {
        let dir = default_config_dir();
        let path = dir.join("bridge.toml");
        let mut cfg = if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            toml::from_str::<BridgeServeConfig>(&contents)
                .map_err(|e| anyhow::anyhow!("Failed to parse bridge.toml: {}", e))?
        } else {
            Self::default()
        };

        // Populate agent_id from common.toml, generating if needed
        let mut common = CommonConfig::load_from_dir(&dir)?;
        common.ensure_agent_id();
        if cfg.agent_id.is_empty() {
            cfg.agent_id = common.agent_id.clone();
        }
        // Persist updated common.toml so the agent_id is stable across restarts
        common.save_to_dir(&dir).ok();

        Ok(cfg)
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
