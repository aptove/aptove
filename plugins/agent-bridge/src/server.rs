//! `BridgeServer` — wires a single `StdioBridge` to an in-process agent transport.
//!
//! `BridgeServer::build()` reads transport settings from [`AgentConfig`],
//! selects the first enabled transport, then starts exactly one bridge listener.
//! The TUI's `services/bridge.rs` handles concurrent multi-transport operation.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use agent_core::config::{AgentConfig, TransportCloudflareConfig, TransportLocalConfig, TransportTailscaleIpConfig, TransportTailscaleServeConfig};
use agent_core::transport::InProcessTransport;
use agent_core::trigger::TriggerStore;
use bridge::bridge::{AgentHandle, StdioBridge, WebhookTarget};
use bridge::cloudflare::{cloudflared_config_path, write_credentials_file, write_cloudflared_config};
use bridge::cloudflared_runner::CloudflaredRunner;
use bridge::pairing::PairingManager;
use bridge::tailscale::{get_tailscale_hostname, get_tailscale_ipv4, tailscale_serve_start, TailscaleServeGuard};
use bridge::tls::TlsConfig;
use bridge::agent_pool::{AgentPool, PoolConfig};

use crate::config::BridgeServeConfig;

// ---------------------------------------------------------------------------
// Daemon handles — kept alive for the server's lifetime
// ---------------------------------------------------------------------------

#[derive(Default)]
struct BridgeDaemons {
    cloudflared_runners: Vec<CloudflaredRunner>,
    tailscale_guards: Vec<TailscaleServeGuard>,
}

// ---------------------------------------------------------------------------
// BridgeServer
// ---------------------------------------------------------------------------

/// Manages one in-process transport and one bridge listener.
///
/// Use [`BridgeServer::take_transport`] to extract the `InProcessTransport` for the agent
/// message loop, then call [`BridgeServer::start`] to run the bridge listener.
pub struct BridgeServer {
    /// In-process transport for the agent loop to use instead of stdin/stdout.
    transport: Option<InProcessTransport>,
    /// The single active bridge listener.
    bridge: StdioBridge,
    /// External daemon handles (cloudflared, tailscale) that must live as long as the bridge.
    _daemons: BridgeDaemons,
    /// Name of the transport that was selected and configured (e.g. "local", "cloudflare").
    selected_transport: String,
    /// Resolved WebSocket hostname for the selected transport.
    hostname: String,
    /// Pairing manager for generating one-time-code pairing QRs.
    pairing_manager: Arc<PairingManager>,
    /// AgentConfig snapshot — for QR payload building.
    agent_config: AgentConfig,
}

impl BridgeServer {
    /// Build the bridge server from `AgentConfig`.
    pub fn build(agent_config: &AgentConfig, bridge_config: &BridgeServeConfig) -> Result<Self> {
        Self::build_with_trigger_store(agent_config, bridge_config, None)
    }

    /// Build with an optional `TriggerStore` for webhook token resolution.
    pub fn build_with_trigger_store(
        agent_config: &AgentConfig,
        bridge_config: &BridgeServeConfig,
        trigger_store: Option<Arc<dyn TriggerStore>>,
    ) -> Result<Self> {
        // bridge → agent (replaces stdin)
        let (stdin_tx, stdin_rx) = mpsc::channel::<Vec<u8>>(256);
        // agent → bridge (replaces stdout)
        let (stdout_tx, stdout_rx) = mpsc::channel::<Vec<u8>>(256);

        let transport = InProcessTransport::new(stdin_rx, stdout_tx);
        let stdout_rx_arc = Arc::new(tokio::sync::Mutex::new(stdout_rx));

        // Build webhook resolver
        let webhook_resolver: Option<bridge::bridge::WebhookResolverFn> =
            trigger_store.map(|store| {
                let store = Arc::clone(&store);
                let resolver: bridge::bridge::WebhookResolverFn =
                    Arc::new(move |token: String| {
                        let store = Arc::clone(&store);
                        Box::pin(async move {
                            match store.resolve_token(&token).await {
                                Ok(Some((workspace_id, trigger))) => Some(WebhookTarget {
                                    workspace_id,
                                    trigger_id: trigger.id,
                                    trigger_name: trigger.name,
                                    rate_limit_per_minute: trigger.rate_limit_per_minute,
                                    hmac_secret: trigger.hmac_secret,
                                    accepted_content_types: trigger.accepted_content_types,
                                }),
                                _ => None,
                            }
                        })
                    });
                resolver
            });

        let agent_id = &agent_config.identity.agent_id;
        let auth_token = &agent_config.identity.auth_token;
        let transports = &agent_config.transports;
        let enabled = transports.enabled_names();

        let agent_handle = AgentHandle::InProcess {
            stdin_tx,
            stdout_rx: stdout_rx_arc,
        };

        // Fall back to local transport if none are explicitly enabled
        if enabled.is_empty() {
            info!("No transports enabled in config; using local defaults");
            let local_cfg = &transports.local;
            let fallback_ip = bridge_config.advertise_addr.clone().unwrap_or_else(|| {
                local_ip_address::local_ip()
                    .map(|a| a.to_string())
                    .unwrap_or_else(|_| "127.0.0.1".to_string())
            });
            let fallback_hostname = format!("wss://{}:{}", fallback_ip, local_cfg.port);
            let fallback_pm = Arc::new(PairingManager::new_with_cf(
                agent_id.clone(),
                fallback_hostname.clone(),
                auth_token.clone(),
                None, None, None,
            ));
            let bridge = build_local_bridge(
                local_cfg,
                agent_handle,
                bridge_config,
                agent_id,
                auth_token,
                webhook_resolver,
            )?;
            return Ok(Self {
                transport: Some(transport),
                bridge,
                _daemons: BridgeDaemons::default(),
                selected_transport: "local".to_string(),
                hostname: fallback_hostname,
                pairing_manager: fallback_pm,
                agent_config: agent_config.clone(),
            });
        }

        // Use the transport selected by the user (stored in state.active_transport).
        // Fall back to the first enabled transport if the selection is invalid.
        let active = agent_config.state.active_transport.as_str();
        let transport_name = if !active.is_empty() && enabled.contains(&active) {
            active
        } else {
            if !active.is_empty() {
                warn!(
                    "active_transport '{}' is not enabled; falling back to '{}'",
                    active, enabled[0]
                );
            }
            enabled[0]
        };

        let config_dir = AgentConfig::data_dir().unwrap_or_else(|_| {
            dirs::config_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("Aptove")
        });

        let (hostname, pm, tls_cfg, ts_guard, cf_runner) = match transport_name {
            "local" => build_transport_local(
                &transports.local,
                agent_id,
                auth_token,
                &config_dir,
                bridge_config.advertise_addr.as_deref(),
            )?,
            "tailscale-serve" => build_transport_tailscale_serve(
                &transports.tailscale_serve,
                agent_id,
                auth_token,
            )?,
            "tailscale-ip" => build_transport_tailscale_ip(
                &transports.tailscale_ip,
                agent_id,
                auth_token,
                &config_dir,
            )?,
            "cloudflare" => build_transport_cloudflare(
                &transports.cloudflare,
                agent_id,
                auth_token,
            )?,
            other => {
                warn!("Unknown transport '{}'; falling back to local", other);
                build_transport_local(
                    &transports.local,
                    agent_id,
                    auth_token,
                    &config_dir,
                    bridge_config.advertise_addr.as_deref(),
                )?
            }
        };

        info!("📡 Transport '{}' → {}", transport_name, hostname);

        let port = match transport_name {
            "tailscale-serve" => transports.tailscale_serve.port,
            "tailscale-ip" => transports.tailscale_ip.port,
            "cloudflare" => transports.local.port, // cloudflare binds locally
            _ => transports.local.port,
        };

        // tailscale-serve proxies from Tailscale edge → localhost
        let effective_bind = if transport_name == "tailscale-serve" {
            "127.0.0.1".to_string()
        } else {
            bridge_config.bind_addr.clone()
        };

        let uses_external_tls = matches!(transport_name, "tailscale-serve" | "cloudflare");

        let mut bridge = StdioBridge::new(String::new(), port)
            .with_agent_handle(agent_handle)
            .with_bind_addr(effective_bind)
            .with_auth_token(Some(auth_token.clone()));

        if let Some(tls) = tls_cfg {
            bridge = bridge.with_tls(tls);
        } else if uses_external_tls {
            bridge = bridge.with_external_tls();
        }

        bridge = bridge.with_pairing(pm);
        let pairing_manager = bridge.pairing_manager()
            .expect("pairing_manager set above")
            .clone();

        if let Some(resolver) = webhook_resolver {
            bridge = bridge.with_webhook_resolver(resolver);
        }

        if bridge_config.keep_alive {
            let pool = Arc::new(tokio::sync::RwLock::new(AgentPool::new(PoolConfig::default())));
            bridge = bridge.with_agent_pool(pool);
        }

        let mut daemons = BridgeDaemons::default();
        if let Some(runner) = cf_runner {
            daemons.cloudflared_runners.push(runner);
        }
        if let Some(guard) = ts_guard {
            daemons.tailscale_guards.push(guard);
        }

        info!("BridgeServer built with transport: {}", transport_name);

        Ok(Self {
            transport: Some(transport),
            bridge,
            _daemons: daemons,
            selected_transport: transport_name.to_string(),
            hostname,
            pairing_manager,
            agent_config: agent_config.clone(),
        })
    }

    /// Name of the transport that was selected and configured.
    pub fn selected_transport_name(&self) -> &str {
        &self.selected_transport
    }

    /// Display the connection QR code using the pairing flow.
    pub fn show_qr(&self) -> anyhow::Result<()> {
        if self.selected_transport == "cloudflare" {
            let cf = &self.agent_config.transports.cloudflare;
            let url = cf.hostname.replacen("https://", "wss://", 1);
            let mut map = serde_json::Map::new();
            map.insert("agentId".into(), serde_json::Value::String(self.agent_config.identity.agent_id.clone()));
            map.insert("protocol".into(), serde_json::Value::String("acp".into()));
            map.insert("version".into(), serde_json::Value::String("1.0".into()));
            map.insert("authToken".into(), serde_json::Value::String(self.agent_config.identity.auth_token.clone()));
            map.insert("url".into(), serde_json::Value::String(url));
            let json = serde_json::to_string(&serde_json::Value::Object(map))?;
            bridge::qr::display_qr_code(&json, "cloudflare")?;
        } else {
            bridge::qr::display_qr_code_with_pairing(&self.hostname, &self.pairing_manager)?;
        }
        Ok(())
    }

    /// Extract the in-process transport for use in the agent message loop.
    ///
    /// Must be called before [`start`]. Panics if called twice.
    pub fn take_transport(&mut self) -> InProcessTransport {
        self.transport.take().expect("transport already taken")
    }

    /// Consume the server and run the bridge listener.
    pub async fn start(self) -> Result<()> {
        let Self { bridge, _daemons, .. } = self;
        bridge.start().await.map_err(|e| anyhow::anyhow!("bridge error: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Local transport (fallback when BridgeServeConfig is used without AgentConfig)
// ---------------------------------------------------------------------------

fn build_local_bridge(
    local_cfg: &TransportLocalConfig,
    agent_handle: AgentHandle,
    bridge_config: &BridgeServeConfig,
    agent_id: &str,
    auth_token: &str,
    webhook_resolver: Option<bridge::bridge::WebhookResolverFn>,
) -> Result<StdioBridge> {
    let config_dir = AgentConfig::data_dir().unwrap_or_else(|_| {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Aptove")
    });

    let extra_sans: Vec<String> = bridge_config.advertise_addr
        .as_deref()
        .map(|a| vec![a.to_string()])
        .unwrap_or_default();

    let tls_config = if local_cfg.tls {
        Some(TlsConfig::load_or_generate(&config_dir, &extra_sans)?)
    } else {
        None
    };
    let cert_fingerprint = tls_config.as_ref().map(|t| t.fingerprint.clone());

    let ip = bridge_config.advertise_addr.clone().unwrap_or_else(|| {
        local_ip_address::local_ip()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    });
    let protocol = if tls_config.is_some() { "wss" } else { "ws" };
    let ws_url = format!("{}://{}:{}", protocol, ip, local_cfg.port);

    let pm = PairingManager::new_with_cf(
        agent_id.to_string(),
        ws_url,
        auth_token.to_string(),
        cert_fingerprint,
        None,
        None,
    );

    let mut bridge = StdioBridge::new(String::new(), local_cfg.port)
        .with_agent_handle(agent_handle)
        .with_bind_addr(bridge_config.bind_addr.clone())
        .with_auth_token(Some(auth_token.to_string()));

    if let Some(tls) = tls_config {
        bridge = bridge.with_tls(tls);
    }

    bridge = bridge.with_pairing(pm);

    if bridge_config.keep_alive {
        let pool = Arc::new(tokio::sync::RwLock::new(AgentPool::new(PoolConfig::default())));
        bridge = bridge.with_agent_pool(pool);
    }

    if let Some(resolver) = webhook_resolver {
        bridge = bridge.with_webhook_resolver(resolver);
    }

    info!(port = local_cfg.port, tls = local_cfg.tls, "BridgeServer built (local transport)");
    Ok(bridge)
}

// ---------------------------------------------------------------------------
// Per-transport initialization
// ---------------------------------------------------------------------------

type TransportResult = Result<(
    String,              // hostname / ws url
    PairingManager,
    Option<TlsConfig>,
    Option<TailscaleServeGuard>,
    Option<CloudflaredRunner>,
)>;

fn build_transport_local(
    cfg: &TransportLocalConfig,
    agent_id: &str,
    auth_token: &str,
    config_dir: &std::path::PathBuf,
    advertise_addr: Option<&str>,
) -> TransportResult {
    let extra_sans: Vec<String> = advertise_addr
        .map(|a| vec![a.to_string()])
        .unwrap_or_default();
    let tls_config = if cfg.tls {
        Some(TlsConfig::load_or_generate(config_dir, &extra_sans)?)
    } else {
        None
    };
    let cert_fingerprint = tls_config.as_ref().map(|t| t.fingerprint.clone());
    let ip = match advertise_addr {
        Some(addr) => addr.to_string(),
        None => local_ip_address::local_ip()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string()),
    };
    let protocol = if tls_config.is_some() { "wss" } else { "ws" };
    let hostname = format!("{}://{}:{}", protocol, ip, cfg.port);
    let pm = PairingManager::new_with_cf(
        agent_id.to_string(),
        hostname.clone(),
        auth_token.to_string(),
        cert_fingerprint,
        None,
        None,
    );
    Ok((hostname, pm, tls_config, None, None))
}

fn build_transport_tailscale_serve(
    cfg: &TransportTailscaleServeConfig,
    agent_id: &str,
    auth_token: &str,
) -> TransportResult {
    let ts_hostname = get_tailscale_hostname()?.ok_or_else(|| {
        anyhow::anyhow!(
            "tailscale-serve mode requires MagicDNS + HTTPS to be enabled on your tailnet."
        )
    })?;
    let hostname = format!("wss://{}", ts_hostname);
    let pm = PairingManager::new_with_cf(
        agent_id.to_string(),
        hostname.clone(),
        auth_token.to_string(),
        None,
        None,
        None,
    )
    .with_tailscale_path();

    info!("🌐 Starting tailscale serve...");
    let guard = tailscale_serve_start(cfg.port)?;
    info!("📡 Tailscale (serve): wss://{}", ts_hostname);

    Ok((hostname, pm, None, Some(guard), None))
}

fn build_transport_tailscale_ip(
    cfg: &TransportTailscaleIpConfig,
    agent_id: &str,
    auth_token: &str,
    config_dir: &std::path::PathBuf,
) -> TransportResult {
    let ts_ip = get_tailscale_ipv4()?;
    let addr = get_tailscale_hostname()?.unwrap_or_else(|| ts_ip.clone());
    let tls_config = Some(TlsConfig::load_or_generate(config_dir, &[ts_ip.clone()])?);
    let cert_fingerprint = tls_config.as_ref().map(|t| t.fingerprint.clone());
    let hostname = format!("wss://{}:{}", addr, cfg.port);
    let pm = PairingManager::new_with_cf(
        agent_id.to_string(),
        hostname.clone(),
        auth_token.to_string(),
        cert_fingerprint,
        None,
        None,
    )
    .with_tailscale_path();
    Ok((hostname, pm, tls_config, None, None))
}

fn build_transport_cloudflare(
    cfg: &TransportCloudflareConfig,
    agent_id: &str,
    auth_token: &str,
) -> TransportResult {
    let hostname = cfg.hostname.clone();
    let pm = PairingManager::new_with_cf(
        agent_id.to_string(),
        hostname.clone(),
        auth_token.to_string(),
        None,
        None,
        None,
    );

    let runner = if !cfg.tunnel_id.is_empty() {
        // Use a fixed local port for cloudflared → bridge connection
        let local_port: u16 = 8765;
        let credentials_path =
            write_credentials_file(&cfg.account_id, &cfg.tunnel_id, &cfg.tunnel_secret)?;
        let hostname_str = hostname.trim_start_matches("https://");
        let _ = write_cloudflared_config(&cfg.tunnel_id, &credentials_path, hostname_str, local_port)?;
        let config_yml = cloudflared_config_path()?;
        info!("🌐 Starting cloudflared tunnel daemon...");
        let mut r = CloudflaredRunner::spawn(&config_yml, &cfg.tunnel_id)?;
        r.wait_for_ready(std::time::Duration::from_secs(30))?;
        info!("🌐 Cloudflare tunnel active: {}", hostname);
        Some(r)
    } else {
        warn!("Cloudflare transport: tunnel_id not configured, skipping cloudflared startup");
        None
    };

    Ok((hostname, pm, None, None, runner))
}
