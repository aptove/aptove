//! `agent-bridge` — Embedded bridge server for `aptove run`.
//!
//! Wires an [`AgentRuntime`] directly to a [`StdioBridge`] via in-process
//! channels, so the agent and bridge server run in the same process with no
//! subprocess spawning or stdio piping.

pub mod config;
pub mod server;

pub use config::BridgeServeConfig;
pub use server::BridgeServer;

// Re-export CommonConfig so agent-cli can use it without a direct bridge dependency.
pub use bridge::common_config::{CommonConfig, TransportConfig};

/// Detect the active bridge transport and display its static connection QR code.
///
/// Probes each enabled transport's listen port. If one accepts a connection the
/// QR is built from credentials in `config` (no server is started, no pairing
/// code is issued). Returns `Ok(true)` when a QR was shown, `Ok(false)` when no
/// transport is currently active (bridge not running).
pub fn show_qr(
    config_dir: &std::path::PathBuf,
    config: &CommonConfig,
) -> anyhow::Result<bool> {
    for (name, cfg) in config.enabled_transports() {
        let default_port: u16 = if name == "tailscale-serve" { 8766 } else { 8765 };
        let port = cfg.port.unwrap_or(default_port);
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        if std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300))
            .is_ok()
        {
            show_transport_qr(name, cfg, config, config_dir)?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Build and display a static connection QR for a specific transport.
/// No server is started — credentials are read from `config` and the cert from disk.
fn show_transport_qr(
    transport_name: &str,
    transport_cfg: &TransportConfig,
    config: &CommonConfig,
    config_dir: &std::path::PathBuf,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use bridge::tailscale::{get_tailscale_hostname, get_tailscale_ipv4};
    use bridge::tls::TlsConfig;

    let default_port: u16 = if transport_name == "tailscale-serve" { 8766 } else { 8765 };
    let port = transport_cfg.port.unwrap_or(default_port);

    let mut map = serde_json::Map::new();
    if !config.agent_id.is_empty() {
        map.insert("agentId".into(), serde_json::Value::String(config.agent_id.clone()));
    }
    map.insert("protocol".into(), serde_json::Value::String("acp".into()));
    map.insert("version".into(), serde_json::Value::String("1.0".into()));
    if !config.auth_token.is_empty() {
        map.insert("authToken".into(), serde_json::Value::String(config.auth_token.clone()));
    }

    match transport_name {
        "cloudflare" => {
            let hostname = transport_cfg.hostname.clone().unwrap_or_default();
            let url = hostname.replacen("https://", "wss://", 1);
            map.insert("url".into(), serde_json::Value::String(url));
            if let Some(id) = transport_cfg.client_id.as_deref().filter(|s| !s.is_empty()) {
                map.insert("clientId".into(), serde_json::Value::String(id.to_string()));
            }
            if let Some(secret) = transport_cfg.client_secret.as_deref().filter(|s| !s.is_empty()) {
                map.insert("clientSecret".into(), serde_json::Value::String(secret.to_string()));
            }
        }
        "tailscale-serve" => {
            let ts_hostname = get_tailscale_hostname()?
                .ok_or_else(|| anyhow::anyhow!("Tailscale MagicDNS hostname not available"))?;
            map.insert("url".into(), serde_json::Value::String(format!("wss://{}", ts_hostname)));
        }
        "tailscale-ip" => {
            let ts_ip = get_tailscale_ipv4()?;
            let addr = get_tailscale_hostname()?.unwrap_or_else(|| ts_ip.clone());
            let tls_config = TlsConfig::load_or_generate(config_dir, &[ts_ip])
                .context("Failed to load TLS config")?;
            map.insert("url".into(), serde_json::Value::String(format!("wss://{}:{}", addr, port)));
            map.insert("certFingerprint".into(), serde_json::Value::String(tls_config.fingerprint));
        }
        _ => {
            // "local" and any unknown name
            let tls_config = TlsConfig::load_or_generate(config_dir, &[])
                .context("Failed to load TLS config")?;
            let ip = match local_ip_address::local_ip() {
                Ok(a) => a.to_string(),
                Err(_) => "127.0.0.1".to_string(),
            };
            map.insert("url".into(), serde_json::Value::String(format!("wss://{}:{}", ip, port)));
            map.insert("certFingerprint".into(), serde_json::Value::String(tls_config.fingerprint));
        }
    }

    let json = serde_json::to_string(&serde_json::Value::Object(map))
        .context("Failed to build connection JSON")?;
    bridge::qr::display_qr_code(&json, transport_name).context("Failed to display QR code")?;
    Ok(())
}
