use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Cached JWT token with expiry tracking.
struct JwtCache {
    token: String,
    expires_at: Instant,
}

/// Push relay client for forwarding device tokens and sending push notifications
/// via the centralized push relay service (Cloudflare Worker).
///
/// The bridge never holds APNs/FCM credentials. It only knows the relay URL
/// and authenticates via a short-lived RS256 JWT fetched from the token service.
#[derive(Clone)]
pub struct PushRelayClient {
    relay_url: String,
    http_client: reqwest::Client,
    /// Per-token debounce tracking: token → last notification time
    debounce: Arc<RwLock<HashMap<String, Instant>>>,
    /// Debounce cooldown duration (default 30s)
    cooldown: Duration,
    /// JWT auth — set by with_jwt_credentials()
    token_url: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    jwt_cache: Arc<RwLock<Option<JwtCache>>>,
    /// When true (default), the notification title is "Aptove" instead of the
    /// agent name, to avoid leaking potentially-sensitive folder names to the
    /// push relay, Apple APNs, or Google FCM.
    generic_title: bool,
    /// Agent UUID for abuse tracking — sent with every token request.
    agent_id: Option<String>,
    /// Server-generated binding secret for credential binding enforcement.
    binding_secret: Option<String>,
    /// SHA-256 hash of OS machine-id for abuse detection.
    device_id: Option<String>,
    /// Callback invoked when the token service returns a binding_secret on first use.
    /// The callback receives the binding_secret to persist to config.
    binding_secret_callback: Option<Arc<dyn Fn(String) + Send + Sync>>,
}

/// Request to register a device token with the relay
#[derive(Debug, Serialize)]
struct RegisterRequest {
    device_token: String,
    platform: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_id: Option<String>,
}

/// Request to unregister a device token
#[derive(Debug, Serialize)]
struct UnregisterRequest {
    device_token: String,
}

/// Request to send a push notification
#[derive(Debug, Serialize)]
struct PushRequest {
    title: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<HashMap<String, String>>,
}

/// Token service response for POST /token
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    /// Returned only on first use when the token service enforces binding.
    binding_secret: Option<String>,
}

/// Push relay API response
#[derive(Debug, Deserialize)]
struct RelayResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl PushRelayClient {
    /// Create a new push relay client.
    ///
    /// - `relay_url`: Base URL of the push relay (e.g., "https://push.aptove.com")
    pub fn new(relay_url: String) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            relay_url: relay_url.trim_end_matches('/').to_string(),
            http_client,
            debounce: Arc::new(RwLock::new(HashMap::new())),
            cooldown: Duration::from_secs(30),
            token_url: None,
            client_id: None,
            client_secret: None,
            jwt_cache: Arc::new(RwLock::new(None)),
            generic_title: true,
            agent_id: None,
            binding_secret: None,
            device_id: None,
            binding_secret_callback: None,
        }
    }

    /// Override whether the notification title is the generic "Aptove" string
    /// (default: `true`).  Set to `false` to use the actual agent name as the
    /// title; note that this sends the agent name to the push relay and from
    /// there to Apple APNs or Google FCM.
    pub fn with_generic_title(mut self, v: bool) -> Self {
        self.generic_title = v;
        self
    }

    /// Set the agent UUID for abuse tracking.
    pub fn with_agent_id(mut self, agent_id: Option<String>) -> Self {
        self.agent_id = agent_id;
        self
    }

    /// Set the binding secret for credential binding enforcement.
    pub fn with_binding_secret(mut self, binding_secret: Option<String>) -> Self {
        self.binding_secret = binding_secret;
        self
    }

    /// Set the device ID (SHA-256 hash of OS machine-id).
    pub fn with_device_id(mut self, device_id: Option<String>) -> Self {
        self.device_id = device_id;
        self
    }

    /// Set a callback invoked when the server returns a binding_secret on first use.
    pub fn with_binding_secret_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        self.binding_secret_callback = Some(Arc::new(cb));
        self
    }

    /// Configure JWT authentication credentials from the token service.
    pub fn with_jwt_credentials(
        mut self,
        token_url: String,
        client_id: String,
        client_secret: String,
    ) -> Self {
        self.token_url = Some(token_url);
        self.client_id = Some(client_id);
        self.client_secret = Some(client_secret);
        self
    }

    /// Fetch (or return cached) a JWT from the token service.
    ///
    /// The token is cached until it has < 60 seconds remaining.
    async fn get_jwt(&self) -> Result<String> {
        // Fast path: return cached token if still valid
        {
            let cache = self.jwt_cache.read().await;
            if let Some(ref c) = *cache {
                if c.expires_at > Instant::now() + Duration::from_secs(60) {
                    return Ok(c.token.clone());
                }
            }
        }

        let token_url = self
            .token_url
            .as_deref()
            .context("token_url not configured")?;
        let client_id = self
            .client_id
            .as_deref()
            .context("client_id not configured")?;
        let client_secret = self
            .client_secret
            .as_deref()
            .context("client_secret not configured")?;

        let url = format!("{}/token", token_url.trim_end_matches('/'));
        let mut body = serde_json::json!({
            "client_id": client_id,
            "client_secret": client_secret,
        });

        // Include abuse-prevention fields (harmless if server ignores them)
        if let Some(ref agent_id) = self.agent_id {
            body["agent_id"] = serde_json::Value::String(agent_id.clone());
        }
        if let Some(ref device_id) = self.device_id {
            body["device_id"] = serde_json::Value::String(device_id.clone());
        }
        if let Some(ref binding_secret) = self.binding_secret {
            body["binding_secret"] = serde_json::Value::String(binding_secret.clone());
        }

        debug!("Fetching JWT from token service: {}", url);
        let res = self
            .http_client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to contact token service")?;

        let status = res.status();
        if !status.is_success() {
            let error_body = res.text().await.unwrap_or_default();
            if error_body.contains("binding_secret_required") {
                error!("Token service requires a binding_secret. This client credential is already bound to another machine. Create a new client in the dashboard.");
            }
            anyhow::bail!("Token service returned HTTP {}: {}", status, error_body);
        }

        let token_resp: TokenResponse = res
            .json()
            .await
            .context("Failed to parse token service response")?;

        // If the server returned a binding_secret (first use), persist it via callback
        if let Some(ref bs) = token_resp.binding_secret {
            info!("Received binding_secret from token service (first use)");
            if let Some(ref cb) = self.binding_secret_callback {
                cb(bs.clone());
            }
        }

        let expires_at = Instant::now()
            + Duration::from_secs(token_resp.expires_in.saturating_sub(60));

        let mut cache = self.jwt_cache.write().await;
        *cache = Some(JwtCache {
            token: token_resp.access_token.clone(),
            expires_at,
        });

        debug!("JWT fetched, expires in {}s", token_resp.expires_in);
        Ok(token_resp.access_token)
    }

    /// Build an HTTP request with JWT Authorization header.
    async fn authorized_request(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        let jwt = self.get_jwt().await?;
        Ok(builder.header("Authorization", format!("Bearer {}", jwt)))
    }

    /// Register a device token with the push relay.
    ///
    /// Called when the mobile app sends `bridge/registerPushToken` over WebSocket.
    pub async fn register_device(
        &self,
        device_token: &str,
        platform: &str,
        bundle_id: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/register", self.relay_url);
        let body = RegisterRequest {
            device_token: device_token.to_string(),
            platform: platform.to_string(),
            bundle_id: bundle_id.map(|s| s.to_string()),
        };

        info!("📱 Registering {} device token with push relay", platform);
        debug!("Push relay URL: {}", url);

        let builder = self.http_client.post(&url).json(&body);
        let builder = self.authorized_request(builder).await?;
        let res = builder
            .send()
            .await
            .context("Failed to contact push relay for registration")?;

        let status = res.status();
        let response: RelayResponse = res
            .json()
            .await
            .context("Failed to parse push relay response")?;

        if response.ok {
            info!("✅ Device token registered with push relay");
            Ok(())
        } else {
            let err_msg = response
                .error
                .or(response.message)
                .unwrap_or_else(|| format!("HTTP {}", status));
            error!("❌ Push relay registration failed: {}", err_msg);
            anyhow::bail!("Push relay registration failed: {}", err_msg)
        }
    }

    /// Unregister a device token from the push relay.
    pub async fn unregister_device(&self, device_token: &str) -> Result<()> {
        let url = format!("{}/register", self.relay_url);
        let body = UnregisterRequest {
            device_token: device_token.to_string(),
        };

        info!("📱 Unregistering device token from push relay");

        let builder = self.http_client.delete(&url).json(&body);
        let builder = self.authorized_request(builder).await?;
        let res = builder
            .send()
            .await
            .context("Failed to contact push relay for unregistration")?;

        let response: RelayResponse = res
            .json()
            .await
            .context("Failed to parse push relay response")?;

        if response.ok {
            info!("✅ Device token unregistered from push relay");
        }
        Ok(())
    }

    /// Send a push notification via the relay.
    ///
    /// Includes per-agent debounce: if a notification was sent within the
    /// cooldown window (default 30s), the new one is silently dropped.
    ///
    /// The notification content is fixed ("Your agent has new activity")
    /// to prevent leaking agent response content.
    pub async fn notify(&self, agent_name: &str) -> Result<bool> {
        // Use client_id as debounce key (unique per bridge identity)
        let debounce_key = self
            .client_id
            .clone()
            .unwrap_or_else(|| self.relay_url.clone());

        // Debounce check
        {
            let debounce = self.debounce.read().await;
            if let Some(last) = debounce.get(&debounce_key) {
                if last.elapsed() < self.cooldown {
                    debug!(
                        "Push notification throttled ({}s remaining)",
                        (self.cooldown - last.elapsed()).as_secs()
                    );
                    return Ok(false);
                }
            }
        }

        // Update debounce timestamp
        {
            let mut debounce = self.debounce.write().await;
            debounce.insert(debounce_key, Instant::now());
        }

        let url = format!("{}/push", self.relay_url);
        // When generic_title is enabled the user has opted out of sending identifying
        // information to APNs/FCM. Omit agentName from the data payload entirely so
        // the agent name does not leak even though the title is obscured.
        let data: Option<HashMap<String, String>> = if self.generic_title {
            None
        } else {
            let mut m = HashMap::new();
            m.insert("agentName".to_string(), agent_name.to_string());
            Some(m)
        };
        let title = if self.generic_title {
            "Aptove".to_string()
        } else {
            agent_name.to_string()
        };
        let body = PushRequest {
            title,
            body: "Your agent has new activity".to_string(),
            data,
        };

        info!("🔔 Sending push notification via relay for agent '{}'", agent_name);

        let builder = self.http_client.post(&url).json(&body);
        let builder = match self.authorized_request(builder).await {
            Ok(b) => b,
            Err(e) => {
                warn!("⚠️  Failed to get JWT for push notification: {}", e);
                return Ok(false);
            }
        };

        let res = builder
            .send()
            .await
            .context("Failed to contact push relay for notification")?;

        let status = res.status();
        let response: RelayResponse = res
            .json()
            .await
            .context("Failed to parse push relay response")?;

        if response.ok {
            info!("✅ Push notification sent via relay");
            Ok(true)
        } else {
            let err_msg = response
                .error
                .or(response.message)
                .unwrap_or_else(|| format!("HTTP {}", status));
            warn!("⚠️  Push relay notification failed: {}", err_msg);
            Ok(false)
        }
    }
}
