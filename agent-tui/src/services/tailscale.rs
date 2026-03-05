//! Tailscale lifecycle service.
//!
//! Polls `tailscale status --json` every 5 seconds and broadcasts parsed
//! status to `AppState` via a `watch::Sender<TailscaleStatus>`.
//!
//! On launch: if `state.tailscale_enabled`, starts `tailscaled` (checks socket
//! first), runs `tailscale set --operator=$USER`, then runs `tailscale up`.

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
pub enum TailscaleStatus {
    Connected { ip: String, hostname: String },
    NeedsLogin { auth_url: String },
    NotRunning,
    NotInstalled,
    Unknown,
}

// ---------------------------------------------------------------------------
// TailscaleService
// ---------------------------------------------------------------------------

pub struct TailscaleService {
    config: AgentConfig,
    tx: watch::Sender<TailscaleStatus>,
    state: Arc<RwLock<AppState>>,
}

impl TailscaleService {
    pub fn new(
        config: AgentConfig,
        tx: watch::Sender<TailscaleStatus>,
        state: Arc<RwLock<AppState>>,
    ) -> Self {
        Self { config, tx, state }
    }

    pub async fn run(self) {
        // Auto-start tailscaled if enabled in persisted state
        if self.config.state.tailscale_enabled {
            self.start_tailscale().await;
        }

        loop {
            let status = self.poll_status().await;
            let _ = self.tx.send(status);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn poll_status(&self) -> TailscaleStatus {
        // Check if tailscale binary exists
        if which::which("tailscale").is_err() {
            return TailscaleStatus::NotInstalled;
        }

        let output = tokio::process::Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            .await;

        match output {
            Err(_) => TailscaleStatus::NotRunning,
            Ok(out) if !out.status.success() => TailscaleStatus::NotRunning,
            Ok(out) => parse_tailscale_status(&String::from_utf8_lossy(&out.stdout)),
        }
    }

    async fn start_tailscale(&self) {
        // Check if tailscaled socket already exists
        let socket_exists = std::path::Path::new("/var/run/tailscale/tailscaled.sock").exists()
            || std::path::Path::new("/run/tailscale/tailscaled.sock").exists();

        if !socket_exists {
            info!("Starting tailscaled...");
            let _ = tokio::process::Command::new("sudo")
                .args(["tailscaled", "--state=/var/lib/tailscale/tailscaled.state"])
                .spawn();
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // Set operator permissions
        let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
        info!("Setting tailscale operator to {}", user);
        let _ = tokio::process::Command::new("sudo")
            .args(["tailscale", "set", &format!("--operator={}", user)])
            .output()
            .await;

        // Bring up tailscale
        info!("Running tailscale up...");
        let up_out = tokio::process::Command::new("tailscale")
            .arg("up")
            .output()
            .await;

        if let Ok(out) = up_out {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("https://login.tailscale.com") {
                // Extract auth URL and broadcast NeedsLogin status
                if let Some(url) = extract_auth_url(&stderr) {
                    let _ = self.tx.send(TailscaleStatus::NeedsLogin { auth_url: url });
                }
            }
        }
    }

    /// Stop tailscale daemon (does NOT call logout).
    pub async fn stop_tailscale(&self) {
        info!("Stopping tailscale...");
        let _ = tokio::process::Command::new("sudo")
            .args(["tailscale", "down"])
            .output()
            .await;
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn parse_tailscale_status(json: &str) -> TailscaleStatus {
    let val: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return TailscaleStatus::NotRunning,
    };

    let backend_state = val["BackendState"].as_str().unwrap_or("");
    match backend_state {
        "Running" => {
            let ip = val["TailscaleIPs"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let hostname = val["Self"]["DNSName"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches('.')
                .to_string();
            TailscaleStatus::Connected { ip, hostname }
        }
        "NeedsLogin" => {
            let auth_url = val["AuthURL"]
                .as_str()
                .unwrap_or("https://login.tailscale.com")
                .to_string();
            TailscaleStatus::NeedsLogin { auth_url }
        }
        "Stopped" | "NoState" => TailscaleStatus::NotRunning,
        _ => TailscaleStatus::NotRunning,
    }
}

fn extract_auth_url(text: &str) -> Option<String> {
    text.lines()
        .find(|l| l.contains("https://login.tailscale.com"))
        .map(|l| l.trim().to_string())
}
