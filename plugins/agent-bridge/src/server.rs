//! `BridgeServer` — wires `StdioBridge` to an in-process agent transport.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

use agent_core::transport::InProcessTransport;
use agent_core::trigger::TriggerStore;
use bridge::bridge::{AgentHandle, StdioBridge, WebhookTarget};
use bridge::tls::TlsConfig;
use bridge::pairing::PairingManager;
use bridge::agent_pool::{AgentPool, PoolConfig};

use crate::config::BridgeServeConfig;

/// Manages the embedded bridge server and its channel connection to the agent.
pub struct BridgeServer {
    /// In-process transport for the agent loop to use instead of stdin/stdout.
    pub transport: InProcessTransport,
    /// The configured bridge server, ready to call `.start().await`.
    pub bridge: StdioBridge,
}

impl BridgeServer {
    /// Build the bridge server and return the paired in-process transport.
    ///
    /// Run both concurrently from `aptove serve`:
    /// ```ignore
    /// let server = BridgeServer::build(&config)?;
    /// tokio::select! {
    ///     _ = agent_loop(server.transport) => {},
    ///     _ = server.bridge.start() => {},
    /// }
    /// ```
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

        let agent_handle = AgentHandle::InProcess {
            stdin_tx,
            stdout_rx: Arc::new(tokio::sync::Mutex::new(stdout_rx)),
        };

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

        // Wire webhook resolver if a TriggerStore was provided
        if let Some(store) = trigger_store {
            let store = Arc::clone(&store);
            let resolver: bridge::bridge::WebhookResolverFn = Arc::new(move |token: String| {
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
            bridge = bridge.with_webhook_resolver(resolver);
            info!("webhook resolver registered");
        }

        info!(port = config.port, tls = config.tls, "BridgeServer built");

        Ok(Self { transport, bridge })
    }
}
