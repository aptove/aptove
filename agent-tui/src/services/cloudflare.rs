//! Cloudflare tunnel lifecycle service.
//!
//! Manages the `cloudflared` process lifecycle. On launch (if enabled), starts
//! `cloudflared tunnel run`. Monitors health, restarts on unexpected exit
//! (up to 3 retries with backoff). Broadcasts status via `watch::Sender`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

use agent_core::config::AgentConfig;

use crate::app::AppState;

// ---------------------------------------------------------------------------
// Status enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum CloudflareStatus {
    Running { hostname: String },
    Stopped,
    Error(String),
    NotConfigured,
    Unknown,
}

// ---------------------------------------------------------------------------
// CloudflareService
// ---------------------------------------------------------------------------

pub struct CloudflareService {
    config: AgentConfig,
    tx: watch::Sender<CloudflareStatus>,
    state: Arc<RwLock<AppState>>,
}

impl CloudflareService {
    pub fn new(
        config: AgentConfig,
        tx: watch::Sender<CloudflareStatus>,
        state: Arc<RwLock<AppState>>,
    ) -> Self {
        Self { config, tx, state }
    }

    pub async fn run(self) {
        let cf = &self.config.transports.cloudflare;

        if cf.tunnel_id.is_empty() {
            let _ = self.tx.send(CloudflareStatus::NotConfigured);
            return;
        }

        if self.config.state.cloudflare_enabled {
            self.start_tunnel().await;
        } else {
            let _ = self.tx.send(CloudflareStatus::Stopped);
        }
    }

    async fn start_tunnel(&self) {
        let cf = &self.config.transports.cloudflare;
        let hostname = cf.hostname.clone();
        let tunnel_id = cf.tunnel_id.clone();

        info!("Starting cloudflared tunnel: {}", hostname);

        let mut retries = 0;
        loop {
            let result = tokio::process::Command::new("cloudflared")
                .args(["tunnel", "run", &tunnel_id])
                .output()
                .await;

            match result {
                Ok(out) if out.status.success() => {
                    let _ = self.tx.send(CloudflareStatus::Running { hostname: hostname.clone() });
                    retries = 0;
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    warn!("cloudflared exited unexpectedly: {}", stderr);
                    if retries >= 3 {
                        let _ = self.tx.send(CloudflareStatus::Error(
                            format!("cloudflared exited after {} retries", retries)
                        ));
                        return;
                    }
                    retries += 1;
                    let backoff = Duration::from_secs(2u64.pow(retries));
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => {
                    warn!("Failed to start cloudflared: {}", e);
                    let _ = self.tx.send(CloudflareStatus::Error(e.to_string()));
                    return;
                }
            }
        }
    }
}
