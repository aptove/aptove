//! `agent-bridge` — Embedded bridge server for `aptove`.
//!
//! Wires an [`AgentRuntime`] directly to a [`StdioBridge`] via in-process
//! channels so the agent and bridge server run in the same process.
//!
//! Transport settings are now read from [`AgentConfig`] rather than a separate
//! `common.toml` file.

pub mod config;
pub mod server;

pub use config::BridgeServeConfig;
pub use server::BridgeServer;

use agent_core::config::AgentConfig;

/// Display the static connection QR for the first active transport.
///
/// Probes each enabled transport's listen port. If one accepts a connection
/// returns `Ok(true)` (aptove already running); otherwise `Ok(false)`.
pub fn show_qr(config: &AgentConfig) -> anyhow::Result<bool> {
    use anyhow::Context as _;
    use bridge::tls::TlsConfig;
    use bridge::tailscale::{get_tailscale_hostname, get_tailscale_ipv4};

    let transports = &config.transports;
    let agent_id = &config.identity.agent_id;
    let auth_token = &config.identity.auth_token;
    let config_dir = AgentConfig::data_dir().unwrap_or_else(|_| {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Aptove")
    });

    for name in transports.enabled_names() {
        let port = match name {
            "tailscale-serve" => transports.tailscale_serve.port,
            "tailscale-ip"   => transports.tailscale_ip.port,
            _                => transports.local.port,
        };
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
            .is_ok()
        {
            // Build and display QR for this transport
            let mut map = serde_json::Map::new();
            if !agent_id.is_empty() {
                map.insert("agentId".into(), serde_json::Value::String(agent_id.clone()));
            }
            map.insert("protocol".into(), serde_json::Value::String("acp".into()));
            map.insert("version".into(), serde_json::Value::String("1.0".into()));
            if !auth_token.is_empty() {
                map.insert("authToken".into(), serde_json::Value::String(auth_token.clone()));
            }

            match name {
                "cloudflare" => {
                    let cf = &transports.cloudflare;
                    let url = cf.hostname.replacen("https://", "wss://", 1);
                    map.insert("url".into(), serde_json::Value::String(url));
                }
                "tailscale-serve" => {
                    let ts_hostname = get_tailscale_hostname()?
                        .ok_or_else(|| anyhow::anyhow!("Tailscale MagicDNS hostname not available"))?;
                    map.insert("url".into(), serde_json::Value::String(format!("wss://{}", ts_hostname)));
                }
                "tailscale-ip" => {
                    let ts_ip = get_tailscale_ipv4()?;
                    let addr_str = get_tailscale_hostname()?.unwrap_or_else(|| ts_ip.clone());
                    let tls_config = TlsConfig::load_or_generate(&config_dir, &[ts_ip])
                        .context("Failed to load TLS config")?;
                    map.insert("url".into(), serde_json::Value::String(
                        format!("wss://{}:{}", addr_str, transports.tailscale_ip.port)
                    ));
                    map.insert("certFingerprint".into(), serde_json::Value::String(tls_config.fingerprint));
                }
                _ => {
                    // local
                    let tls_config = TlsConfig::load_or_generate(&config_dir, &[])
                        .context("Failed to load TLS config")?;
                    let ip = local_ip_address::local_ip()
                        .map(|a| a.to_string())
                        .unwrap_or_else(|_| "127.0.0.1".to_string());
                    map.insert("url".into(), serde_json::Value::String(
                        format!("wss://{}:{}", ip, transports.local.port)
                    ));
                    map.insert("certFingerprint".into(), serde_json::Value::String(tls_config.fingerprint));
                }
            }

            let json = serde_json::to_string(&serde_json::Value::Object(map))
                .context("Failed to build connection JSON")?;
            bridge::qr::display_qr_code(&json, name).context("Failed to display QR code")?;
            return Ok(true);
        }
    }
    Ok(false)
}
