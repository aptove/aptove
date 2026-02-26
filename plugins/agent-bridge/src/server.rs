//! `BridgeServer` — wires a single `StdioBridge` to an in-process agent transport.
//!
//! `BridgeServer::build()` reads `CommonConfig` from the config directory, prompts the user
//! to select a transport when multiple are enabled, then starts exactly one bridge listener.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use agent_core::transport::InProcessTransport;
use agent_core::trigger::TriggerStore;
use bridge::bridge::{AgentHandle, StdioBridge, WebhookTarget};
use bridge::cloudflare::{cloudflared_config_path, write_credentials_file, write_cloudflared_config};
use bridge::cloudflared_runner::CloudflaredRunner;
use bridge::common_config::{CommonConfig, TransportConfig};
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
    /// Take this with [`take_transport`] before calling [`start`].
    transport: Option<InProcessTransport>,
    /// The single active bridge listener.
    bridge: StdioBridge,
    /// External daemon handles (cloudflared, tailscale) that must live as long as the bridge.
    _daemons: BridgeDaemons,
    /// Name of the transport that was selected and configured (e.g. "local", "cloudflare").
    selected_transport: String,
    /// Resolved WebSocket hostname for the selected transport (e.g. "wss://192.168.1.1:8765").
    hostname: String,
    /// Pairing manager for generating one-time-code pairing QRs.
    pairing_manager: Arc<PairingManager>,
    /// CommonConfig snapshot used when building — for Cloudflare QR payloads.
    common_config: CommonConfig,
}

impl BridgeServer {
    /// Build the bridge server and return the paired in-process transport.
    pub fn build(config: &BridgeServeConfig) -> Result<Self> {
        Self::build_with_trigger_store(config, None)
    }

    /// Build with an optional `TriggerStore` for webhook token resolution.
    pub fn build_with_trigger_store(
        config: &BridgeServeConfig,
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

        // Load CommonConfig to get multi-transport settings
        let mut common = CommonConfig::load_from_dir(&config.config_dir).unwrap_or_default();
        common.ensure_agent_id();
        common.ensure_auth_token();
        // Persist so agent_id / auth_token are stable across restarts
        common.save_to_dir(&config.config_dir).ok();

        let enabled_transports: Vec<(String, TransportConfig)> = common
            .enabled_transports()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();

        // Fall back to single-transport from BridgeServeConfig if common.toml has none
        if enabled_transports.is_empty() {
            info!("No transports enabled in common.toml; using BridgeServeConfig defaults");
            let agent_handle = AgentHandle::InProcess {
                stdin_tx,
                stdout_rx: stdout_rx_arc,
            };
            let fallback_ip = local_ip_address::local_ip()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            let fallback_hostname = format!("wss://{}:{}", fallback_ip, config.port);
            let fallback_pm = Arc::new(PairingManager::new_with_cf(
                common.agent_id.clone(),
                fallback_hostname.clone(),
                common.auth_token.clone(),
                None, None, None,
            ));
            let bridge = build_single_bridge_from_config(
                config,
                agent_handle,
                webhook_resolver,
            )?;
            return Ok(Self {
                transport: Some(transport),
                bridge,
                _daemons: BridgeDaemons::default(),
                selected_transport: "local".to_string(),
                hostname: fallback_hostname,
                pairing_manager: fallback_pm,
                common_config: common,
            });
        }

        // When more than one transport is enabled, ask the user to pick one.
        let (transport_name, transport_cfg) = if enabled_transports.len() == 1 {
            enabled_transports.into_iter().next().unwrap()
        } else {
            use std::io::Write as _;
            eprintln!("\nMultiple transports are enabled. Select one to start:");
            for (i, (name, _)) in enabled_transports.iter().enumerate() {
                eprintln!("  [{}] {}", i + 1, name);
            }
            eprint!("Enter number [1]: ");
            std::io::stderr().flush().ok();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).ok();
            let choice: usize = input.trim().parse().unwrap_or(1);
            let idx = choice.saturating_sub(1).min(enabled_transports.len() - 1);
            enabled_transports.into_iter().nth(idx).unwrap()
        };

        let agent_id = common.agent_id.clone();
        let auth_token = common.auth_token.clone();

        // tailscale-serve defaults to 8766 to avoid conflicting with local (8765).
        let default_port: u16 = if transport_name == "tailscale-serve" { 8766 } else { config.port };
        let port = transport_cfg.port.unwrap_or(default_port);
        let use_tls = transport_cfg.tls.unwrap_or(config.tls);

        // tailscale-serve proxies from Tailscale edge → localhost, so bind
        // to 127.0.0.1 only; all other transports use the configured bind addr.
        let effective_bind = if transport_name == "tailscale-serve" {
            "127.0.0.1".to_string()
        } else {
            config.bind_addr.clone()
        };

        let agent_handle = AgentHandle::InProcess {
            stdin_tx,
            stdout_rx: stdout_rx_arc,
        };

        let (hostname, pm, tls_cfg, ts_guard, cf_runner) = build_transport_for_library(
            &transport_name,
            &transport_cfg,
            &agent_id,
            &auth_token,
            port,
            use_tls,
            &config.config_dir,
        )?;

        info!("📡 Transport '{}' → {}", transport_name, hostname);

        let uses_external_tls = matches!(transport_name.as_str(), "tailscale-serve" | "cloudflare");

        let mut bridge = StdioBridge::new(String::new(), port)
            .with_agent_handle(agent_handle)
            .with_bind_addr(effective_bind)
            .with_auth_token(Some(auth_token.clone()));

        if let Some(tls) = tls_cfg {
            bridge = bridge.with_tls(tls);
        } else if uses_external_tls {
            bridge = bridge.with_external_tls();
        }

        // with_pairing wraps pm in Arc internally; retrieve it back for show_qr().
        bridge = bridge.with_pairing(pm);
        let pairing_manager = bridge.pairing_manager()
            .expect("pairing_manager set above")
            .clone();

        if let Some(resolver) = webhook_resolver {
            bridge = bridge.with_webhook_resolver(resolver);
        }

        if config.keep_alive {
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
            selected_transport: transport_name,
            hostname,
            pairing_manager,
            common_config: common,
        })
    }

    /// Name of the transport that was selected and configured (e.g. "local", "cloudflare").
    pub fn selected_transport_name(&self) -> &str {
        &self.selected_transport
    }

    /// Display the connection QR code using the same pairing flow as `bridge run --qr`.
    ///
    /// - For non-Cloudflare transports: shows a one-time-code pairing QR. The iOS
    ///   app calls the pairing endpoint, completes the handshake, and receives the
    ///   `agentId`. This allows the app to deduplicate agents across transports.
    /// - For Cloudflare: shows a static connection QR (no pairing server needed).
    pub fn show_qr(&self) -> anyhow::Result<()> {
        if self.selected_transport == "cloudflare" {
            let json = self.common_config.to_connection_json(&self.hostname, "cloudflare")?;
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
    ///
    /// Keeps daemon processes (cloudflared, tailscale) alive for the duration.
    pub async fn start(self) -> Result<()> {
        let Self { bridge, _daemons, .. } = self;
        // _daemons stays alive until this async fn returns
        bridge.start().await.map_err(|e| anyhow::anyhow!("bridge error: {}", e))
    }
}

// ---------------------------------------------------------------------------
// Single-bridge fallback (when no common.toml transports are configured)
// ---------------------------------------------------------------------------

fn build_single_bridge_from_config(
    config: &BridgeServeConfig,
    agent_handle: AgentHandle,
    webhook_resolver: Option<bridge::bridge::WebhookResolverFn>,
) -> Result<StdioBridge> {
    let mut bridge = StdioBridge::new(String::new(), config.port)
        .with_agent_handle(agent_handle)
        .with_bind_addr(config.bind_addr.clone());

    if let Some(ref token) = config.auth_token {
        bridge = bridge.with_auth_token(Some(token.clone()));
    }

    let tls_fingerprint = if config.tls {
        let tls = TlsConfig::load_or_generate(&config.config_dir, &[])?;
        let fp = tls.fingerprint.clone();
        bridge = bridge.with_tls(tls);
        Some(fp)
    } else {
        None
    };

    let auth_token_str = config.auth_token.clone().unwrap_or_default();
    let ws_url = if config.tls {
        format!("wss://localhost:{}", config.port)
    } else {
        format!("ws://localhost:{}", config.port)
    };
    let pairing = PairingManager::new_with_cf(
        config.agent_id.clone(),
        ws_url,
        auth_token_str,
        tls_fingerprint,
        None,
        None,
    );
    bridge = bridge.with_pairing(pairing);

    if config.keep_alive {
        let pool = Arc::new(tokio::sync::RwLock::new(AgentPool::new(PoolConfig::default())));
        bridge = bridge.with_agent_pool(pool);
    }

    if let Some(resolver) = webhook_resolver {
        bridge = bridge.with_webhook_resolver(resolver);
        info!("webhook resolver registered");
    }

    info!(port = config.port, tls = config.tls, "BridgeServer built (single transport, legacy)");
    Ok(bridge)
}

// ---------------------------------------------------------------------------
// Per-transport initialization (library mode)
// ---------------------------------------------------------------------------

/// Initialize transport-specific infrastructure for library/embedded mode.
///
/// Returns `(hostname, pairing_manager, tls_config, tailscale_guard, cloudflared_runner)`.
fn build_transport_for_library(
    transport_name: &str,
    transport_cfg: &TransportConfig,
    agent_id: &str,
    auth_token: &str,
    port: u16,
    use_tls: bool,
    config_dir: &std::path::PathBuf,
) -> Result<(
    String,
    PairingManager,
    Option<TlsConfig>,
    Option<TailscaleServeGuard>,
    Option<CloudflaredRunner>,
)> {
    match transport_name {
        "cloudflare" => {
            let hostname = transport_cfg.hostname.clone().unwrap_or_default();
            let client_id = transport_cfg.client_id.clone();
            let client_secret = transport_cfg.client_secret.clone();

            let pm = PairingManager::new_with_cf(
                agent_id.to_string(),
                hostname.clone(),
                auth_token.to_string(),
                None,
                client_id,
                client_secret,
            );

            let tunnel_id = transport_cfg.tunnel_id.clone().unwrap_or_default();
            let runner = if !tunnel_id.is_empty() {
                let account_id = transport_cfg.account_id.clone().unwrap_or_default();
                let tunnel_secret = transport_cfg.tunnel_secret.clone().unwrap_or_default();
                let credentials_path =
                    write_credentials_file(&account_id, &tunnel_id, &tunnel_secret)?;
                let hostname_str = hostname.trim_start_matches("https://");
                let _ = write_cloudflared_config(&tunnel_id, &credentials_path, hostname_str, port)?;
                let config_yml = cloudflared_config_path()?;
                info!("🌐 Starting cloudflared tunnel daemon...");
                let mut runner = CloudflaredRunner::spawn(&config_yml, &tunnel_id)?;
                runner.wait_for_ready(std::time::Duration::from_secs(30))?;
                info!("🌐 Cloudflare tunnel active: {}", hostname);
                Some(runner)
            } else {
                warn!(
                    "Cloudflare transport: tunnel_id not configured, skipping cloudflared startup"
                );
                None
            };

            Ok((hostname, pm, None, None, runner))
        }

        "tailscale-serve" => {
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
            let guard = tailscale_serve_start(port)?;
            info!("📡 Tailscale (serve): wss://{}", ts_hostname);

            Ok((hostname, pm, None, Some(guard), None))
        }

        "tailscale-ip" => {
            let ts_ip = get_tailscale_ipv4()?;
            let addr = get_tailscale_hostname()?.unwrap_or_else(|| ts_ip.clone());
            let extra_sans = vec![ts_ip];

            let tls_config = if use_tls {
                Some(TlsConfig::load_or_generate(config_dir, &extra_sans)?)
            } else {
                None
            };
            let cert_fingerprint = tls_config.as_ref().map(|t| t.fingerprint.clone());
            let protocol = if tls_config.is_some() { "wss" } else { "ws" };
            let hostname = format!("{}://{}:{}", protocol, addr, port);

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

        _ => {
            // "local" and any unknown transports — local network with self-signed TLS
            let tls_config = if use_tls {
                Some(TlsConfig::load_or_generate(config_dir, &[])?)
            } else {
                None
            };
            let cert_fingerprint = tls_config.as_ref().map(|t| t.fingerprint.clone());
            let ip = match local_ip_address::local_ip() {
                Ok(addr) => addr.to_string(),
                Err(_) => "127.0.0.1".to_string(),
            };
            let protocol = if tls_config.is_some() { "wss" } else { "ws" };
            let hostname = format!("{}://{}:{}", protocol, ip, port);

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
    }
}
