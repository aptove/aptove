//! ACP bridge service task (TUI mode).
//!
//! Resolves the URL for the single active transport (from `state.active_transport`)
//! and waits for any required daemon (Tailscale, Cloudflare) to become ready
//! before updating `AppState.active_transports` for the Connect tab.
//!
//! The actual bridge listener is started by `run_tui_mode()` in `agent-cli`.
//! This service only tracks transport readiness and the URL shown in the UI.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, RwLock};
use tracing::{info, warn};

use agent_core::config::AgentConfig;

use crate::app::AppState;
use crate::services::cloudflare::CloudflareStatus;
use crate::services::tailscale::TailscaleStatus;

pub struct BridgeService {
    config: AgentConfig,
    state: Arc<RwLock<AppState>>,
    ts_rx: watch::Receiver<TailscaleStatus>,
    cf_rx: watch::Receiver<CloudflareStatus>,
}

impl BridgeService {
    pub fn new(
        config: AgentConfig,
        state: Arc<RwLock<AppState>>,
        ts_rx: watch::Receiver<TailscaleStatus>,
        cf_rx: watch::Receiver<CloudflareStatus>,
    ) -> Self {
        Self { config, state, ts_rx, cf_rx }
    }

    /// Wait for the active transport to be ready, then publish its URL.
    pub async fn run(mut self) {
        let active = self.config.state.active_transport.clone();
        info!("Bridge service: waiting for active transport '{}'", active);

        let url = match active.as_str() {
            "tailscale-serve" | "tailscale-ip" => {
                self.wait_for_tailscale(&active).await
            }
            "cloudflare" => {
                self.wait_for_cloudflare().await
            }
            _ => {
                // local — available immediately
                self.local_url()
            }
        };

        match url {
            Some(u) => {
                info!("Active transport '{}' ready: {}", active, u);
                let mut st = self.state.write().await;
                st.active_transports = vec![(active.clone(), u.clone())];
                st.config.state.active_transport_url = u;
            }
            None => {
                warn!("Active transport '{}' did not become ready; falling back to local", active);
                let fallback = self.local_url().unwrap_or_else(|| "wss://127.0.0.1:8765".to_string());
                let mut st = self.state.write().await;
                st.active_transports = vec![("local".to_string(), fallback.clone())];
                st.config.state.active_transport_url = fallback;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Readiness waiters
    // -----------------------------------------------------------------------

    /// Wait (up to 30 s) for Tailscale to report Connected, then derive the URL.
    async fn wait_for_tailscale(&mut self, transport: &str) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            {
                let status = self.ts_rx.borrow().clone();
                if let TailscaleStatus::Connected { ip, hostname } = &status {
                    let url = match transport {
                        "tailscale-serve" => format!("wss://{}", hostname),
                        _ => {
                            let port = self.config.transports.tailscale_ip.port;
                            format!("wss://{}:{}", ip, port)
                        }
                    };
                    return Some(url);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                warn!("Tailscale did not connect within 30 s");
                return None;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = self.ts_rx.changed().await;
        }
    }

    /// Wait (up to 30 s) for Cloudflare tunnel to report Running, then return the URL.
    async fn wait_for_cloudflare(&mut self) -> Option<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            {
                let status = self.cf_rx.borrow().clone();
                if let CloudflareStatus::Running { hostname } = &status {
                    let url = hostname.replacen("https://", "wss://", 1);
                    return Some(url);
                }
                if matches!(status, CloudflareStatus::NotConfigured | CloudflareStatus::Error(_)) {
                    return None;
                }
            }
            if tokio::time::Instant::now() >= deadline {
                warn!("Cloudflare tunnel did not become ready within 30 s");
                return None;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = self.cf_rx.changed().await;
        }
    }

    fn local_url(&self) -> Option<String> {
        let port = self.config.transports.local.port;
        let ip = local_ip_address::local_ip()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string());
        Some(format!("wss://{}:{}", ip, port))
    }
}
