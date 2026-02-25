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

/// Display a static QR code for a locally-running agent.
///
/// Loads the TLS cert fingerprint from `config_dir`, discovers the local IP,
/// and renders the QR code with `agentId`, `url`, `authToken`, and
/// `certFingerprint` fields.
///
/// Returns an error if the TLS cert cannot be loaded/generated.
pub fn show_qr_for_local_agent(
    config_dir: &std::path::PathBuf,
    config: &CommonConfig,
    port: u16,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use bridge::tls::TlsConfig;

    let tls_config = TlsConfig::load_or_generate(config_dir, &[])
        .context("Failed to load TLS config")?;

    let ip = match local_ip_address::local_ip() {
        Ok(addr) => addr.to_string(),
        Err(_) => "127.0.0.1".to_string(),
    };
    let hostname = format!("wss://{}:{}", ip, port);

    let json = {
        use serde_json::{Map, Value};
        let mut map = Map::new();
        if !config.agent_id.is_empty() {
            map.insert("agentId".to_string(), Value::String(config.agent_id.clone()));
        }
        map.insert("url".to_string(), Value::String(hostname));
        map.insert("protocol".to_string(), Value::String("acp".to_string()));
        map.insert("version".to_string(), Value::String("1.0".to_string()));
        if !config.auth_token.is_empty() {
            map.insert(
                "authToken".to_string(),
                Value::String(config.auth_token.clone()),
            );
        }
        map.insert(
            "certFingerprint".to_string(),
            Value::String(tls_config.fingerprint.clone()),
        );
        serde_json::to_string(&Value::Object(map)).context("Failed to build connection JSON")?
    };

    bridge::qr::display_qr_code(&json, "local").context("Failed to display QR code")?;
    Ok(())
}
