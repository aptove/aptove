//! ACP bridge service task.
//!
//! Starts ALL enabled transports concurrently (not one-or-none).
//! Updates `AppState.active_transports` and `AppState.connected_clients`.
//!
//! NOTE: Full concurrent multi-transport implementation is in Phase 3.
//! This module provides the skeleton and single-transport path.

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

use agent_core::config::AgentConfig;

use crate::app::AppState;

pub struct BridgeService {
    config: AgentConfig,
    state: Arc<RwLock<AppState>>,
}

impl BridgeService {
    pub fn new(config: AgentConfig, state: Arc<RwLock<AppState>>) -> Self {
        Self { config, state }
    }

    /// Start all enabled transports and track active transport URLs.
    pub async fn run(self) {
        let enabled = self.config.transports.enabled_names();

        // Populate active_transports in AppState
        let mut urls = Vec::new();
        for name in &enabled {
            let url = self.transport_url(name);
            if let Some(u) = url {
                info!("Transport '{}' → {}", name, u);
                urls.push((name.to_string(), u));
            }
        }

        self.state.write().await.active_transports = urls;

        // TODO: Phase 3 — actually start bridge listeners and monitor clients
    }

    fn transport_url(&self, name: &str) -> Option<String> {
        let ts = &self.config.transports;
        match name {
            "local" => {
                let ip = local_ip_address::local_ip()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "127.0.0.1".to_string());
                Some(format!("wss://{}:{}", ip, ts.local.port))
            }
            "tailscale-serve" => Some("wss://<tailscale-hostname>".to_string()),
            "tailscale-ip"    => Some("wss://<tailscale-ip>".to_string()),
            "cloudflare"      => {
                if ts.cloudflare.hostname.is_empty() {
                    None
                } else {
                    Some(ts.cloudflare.hostname.replacen("https://", "wss://", 1))
                }
            }
            _ => None,
        }
    }
}
