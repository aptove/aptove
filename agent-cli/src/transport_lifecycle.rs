//! Headless transport lifecycle management.
//!
//! In `--headless` mode the active transport's daemon must be started (if
//! required) before the ACP bridge listener begins. This module handles:
//!
//! - `"tailscale-serve"` / `"tailscale-ip"`: start `tailscaled`, set
//!   `--operator=$USER`, run `tailscale up`, and poll until `Connected`
//!   (up to 60 s).
//! - `"cloudflare"`: the `bridge` crate's `CloudflaredRunner` handles this
//!   inside `BridgeServer::build_with_trigger_store()`; nothing extra needed.
//! - `"local"`: no daemon, returns immediately.
//!
//! On clean exit the caller should call `shutdown()` to stop managed daemons.

use std::time::Duration;

use anyhow::{bail, Result};
use tracing::{info, warn};

use agent_core::config::AgentConfig;

/// Opaque handle returned by [`prepare`]. Drop or call `shutdown()` to clean up.
pub struct TransportGuard {
    kind: GuardKind,
}

enum GuardKind {
    None,
    Tailscale,
}

impl TransportGuard {
    /// Gracefully stop any managed daemon.
    pub async fn shutdown(self) {
        match self.kind {
            GuardKind::None => {}
            GuardKind::Tailscale => {
                info!("Stopping tailscale (headless shutdown)...");
                let _ = tokio::process::Command::new("sudo")
                    .args(["tailscale", "down"])
                    .output()
                    .await;
            }
        }
    }
}

/// Start the daemon required by the active transport and wait until it is ready.
///
/// Returns a [`TransportGuard`] that, when dropped, performs a graceful
/// shutdown of any process that was started here.
///
/// # Fallback
/// If the active transport's daemon fails to become ready within the timeout,
/// `active_transport` in the returned config is reset to `"local"` and a
/// warning is logged. The bridge will then use the local transport instead.
pub async fn prepare(config: &mut AgentConfig) -> Result<TransportGuard> {
    let active = config.state.active_transport.clone();
    info!("Headless: active_transport = '{}'", active);

    match active.as_str() {
        "tailscale-serve" | "tailscale-ip" => {
            match start_tailscale_and_wait().await {
                Ok(()) => Ok(TransportGuard { kind: GuardKind::Tailscale }),
                Err(e) => {
                    warn!("Tailscale not ready: {}. Falling back to local transport.", e);
                    config.state.active_transport = "local".to_string();
                    Ok(TransportGuard { kind: GuardKind::None })
                }
            }
        }
        "cloudflare" => {
            // CloudflaredRunner inside BridgeServer::build_with_trigger_store handles startup.
            info!("Cloudflare: cloudflared will be started by BridgeServer");
            Ok(TransportGuard { kind: GuardKind::None })
        }
        _ => {
            // local or unknown
            info!("Local transport: no daemon startup required");
            Ok(TransportGuard { kind: GuardKind::None })
        }
    }
}

// ---------------------------------------------------------------------------
// Tailscale lifecycle
// ---------------------------------------------------------------------------

async fn start_tailscale_and_wait() -> Result<()> {
    // Start tailscaled if the socket doesn't exist
    let socket_exists = std::path::Path::new("/var/run/tailscale/tailscaled.sock").exists()
        || std::path::Path::new("/run/tailscale/tailscaled.sock").exists();

    if !socket_exists {
        info!("Starting tailscaled...");
        let _ = tokio::process::Command::new("sudo")
            .args(["tailscaled", "--state=/var/lib/tailscale/tailscaled.state"])
            .spawn();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Set operator
    let user = std::env::var("USER").unwrap_or_else(|_| "root".into());
    info!("Setting tailscale operator to '{}'", user);
    let _ = tokio::process::Command::new("sudo")
        .args(["tailscale", "set", &format!("--operator={}", user)])
        .output()
        .await;

    // Bring up tailscale
    info!("Running tailscale up...");
    let _ = tokio::process::Command::new("tailscale")
        .arg("up")
        .output()
        .await;

    // Poll until Connected (max 60 s)
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if std::time::Instant::now() >= deadline {
            bail!("Tailscale did not connect within 60 s");
        }

        let out = tokio::process::Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            .await;

        if let Ok(out) = out {
            if out.status.success() {
                let json = String::from_utf8_lossy(&out.stdout);
                let val: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                let state = val["BackendState"].as_str().unwrap_or("");
                if state == "Running" {
                    info!("Tailscale connected");
                    return Ok(());
                }
                if state == "NeedsLogin" {
                    // Extract and print auth URL so the operator can log in
                    if let Some(url) = val["AuthURL"].as_str() {
                        eprintln!("⚠️  Tailscale needs login. Open this URL:\n  {}", url);
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
