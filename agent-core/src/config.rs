//! Configuration
//!
//! TOML-based configuration: provider selection, API keys, model defaults,
//! MCP server definitions, transport settings, identity, and persisted state.
//! All settings live in a single `config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

// ---------------------------------------------------------------------------
// Configuration structures
// ---------------------------------------------------------------------------

/// Top-level agent configuration (maps to TOML).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Active provider name (e.g. "claude", "gemini", "openai").
    #[serde(default = "default_provider")]
    pub provider: String,

    /// Provider-specific configurations.
    #[serde(default)]
    pub providers: ProvidersConfig,

    /// MCP server definitions.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,

    /// Agent loop settings.
    #[serde(default)]
    pub agent: AgentSettings,

    /// System prompt settings.
    #[serde(default)]
    pub system_prompt: SystemPromptConfig,

    /// Bridge / serve mode settings (bind_addr, advertise_addr, keep_alive).
    #[serde(default)]
    pub serve: ServeConfig,

    /// Stable agent identity — agent_id and auth_token.
    /// OWNED by global config.toml; silently stripped from workspace overlays.
    #[serde(default)]
    pub identity: IdentityConfig,

    /// Network transport configurations (local, tailscale-serve, tailscale-ip, cloudflare).
    /// OWNED by global config.toml; silently stripped from workspace overlays.
    #[serde(default)]
    pub transports: TransportsConfig,

    /// Persisted TUI state (last active tab, service enabled flags).
    /// OWNED by global config.toml; silently stripped from workspace overlays.
    #[serde(default)]
    pub state: AppStateConfig,
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Stable agent identity. Owned by global config — never overrideable per workspace.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityConfig {
    /// UUID v4 agent identifier, generated on first use and persisted.
    #[serde(default)]
    pub agent_id: String,
    /// Bearer token ACP clients must supply to connect. Generated on first use.
    #[serde(default)]
    pub auth_token: String,
}

// ---------------------------------------------------------------------------
// Transport configurations
// ---------------------------------------------------------------------------

/// Local network transport — TLS self-signed cert, LAN IP detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportLocalConfig {
    /// Whether the local transport is enabled (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// WebSocket port (default 8765).
    #[serde(default = "default_local_port")]
    pub port: u16,
    /// Enable TLS with a self-signed certificate (default true).
    #[serde(default = "default_true")]
    pub tls: bool,
}

impl Default for TransportLocalConfig {
    fn default() -> Self {
        Self { enabled: true, port: default_local_port(), tls: true }
    }
}

/// Tailscale-serve transport — proxied via Tailscale edge with MagicDNS HTTPS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportTailscaleServeConfig {
    /// Whether the tailscale-serve transport is enabled (default false).
    #[serde(default)]
    pub enabled: bool,
    /// Local port that Tailscale Serve proxies to (default 8766).
    #[serde(default = "default_ts_serve_port")]
    pub port: u16,
}

impl Default for TransportTailscaleServeConfig {
    fn default() -> Self {
        Self { enabled: false, port: default_ts_serve_port() }
    }
}

/// Tailscale-IP transport — direct Tailscale IP with TLS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportTailscaleIpConfig {
    /// Whether the tailscale-ip transport is enabled (default false).
    #[serde(default)]
    pub enabled: bool,
    /// WebSocket port (default 8765).
    #[serde(default = "default_local_port")]
    pub port: u16,
}

impl Default for TransportTailscaleIpConfig {
    fn default() -> Self {
        Self { enabled: false, port: default_local_port() }
    }
}

/// Cloudflare Tunnel transport.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportCloudflareConfig {
    /// Whether the Cloudflare transport is enabled (default false).
    #[serde(default)]
    pub enabled: bool,
    /// Public hostname for the tunnel (e.g. "https://agent.example.com").
    #[serde(default)]
    pub hostname: String,
    /// Cloudflare Tunnel UUID.
    #[serde(default)]
    pub tunnel_id: String,
    /// Cloudflare account ID.
    #[serde(default)]
    pub account_id: String,
    /// Tunnel secret (base64).
    #[serde(default)]
    pub tunnel_secret: String,
}

/// All transport configurations.
/// Owned by global config.toml; silently stripped from workspace overlays.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransportsConfig {
    /// Local network transport (TLS + LAN IP).
    #[serde(default)]
    pub local: TransportLocalConfig,
    /// Tailscale-serve transport (proxied via Tailscale edge).
    #[serde(rename = "tailscale-serve", default)]
    pub tailscale_serve: TransportTailscaleServeConfig,
    /// Tailscale-IP transport (direct Tailscale IP).
    #[serde(rename = "tailscale-ip", default)]
    pub tailscale_ip: TransportTailscaleIpConfig,
    /// Cloudflare Tunnel transport.
    #[serde(default)]
    pub cloudflare: TransportCloudflareConfig,
}

impl TransportsConfig {
    /// Returns the names of all enabled transports.
    pub fn enabled_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.local.enabled { names.push("local"); }
        if self.tailscale_serve.enabled { names.push("tailscale-serve"); }
        if self.tailscale_ip.enabled { names.push("tailscale-ip"); }
        if self.cloudflare.enabled { names.push("cloudflare"); }
        names
    }
}

// ---------------------------------------------------------------------------
// Persisted TUI state
// ---------------------------------------------------------------------------

/// Persisted TUI state — written to config.toml on exit, restored on launch.
/// Owned by global config.toml; silently stripped from workspace overlays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppStateConfig {
    /// The tab that was active when Aptove last exited (default "Setup").
    #[serde(default = "default_last_tab")]
    pub last_tab: String,
    /// Whether the Tailscale daemon should start on launch.
    #[serde(default)]
    pub tailscale_enabled: bool,
    /// Whether the Cloudflare tunnel should start on launch.
    #[serde(default)]
    pub cloudflare_enabled: bool,
    /// The currently selected transport. One of: "local", "tailscale-serve",
    /// "tailscale-ip", "cloudflare". Both TUI and headless modes use this to
    /// determine which single transport to start. Default: "local".
    #[serde(default = "default_active_transport")]
    pub active_transport: String,
    /// Resolved WebSocket URL of the active transport, populated after the
    /// bridge starts. Used by the Connect tab QR code. Empty until connected.
    #[serde(default)]
    pub active_transport_url: String,
}

impl Default for AppStateConfig {
    fn default() -> Self {
        Self {
            last_tab: default_last_tab(),
            tailscale_enabled: false,
            cloudflare_enabled: false,
            active_transport: default_active_transport(),
            active_transport_url: String::new(),
        }
    }
}

fn default_last_tab() -> String { "Setup".to_string() }
fn default_active_transport() -> String { "local".to_string() }
fn default_true() -> bool { true }
fn default_local_port() -> u16 { 8765 }
fn default_ts_serve_port() -> u16 { 8766 }

fn default_provider() -> String {
    "claude".to_string()
}

/// Generate a random 32-byte hex auth token.
fn generate_token() -> String {
    use rand::Rng as _;
    let bytes: [u8; 32] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Per-provider settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub claude: Option<ProviderConfig>,
    #[serde(default)]
    pub gemini: Option<ProviderConfig>,
    #[serde(default)]
    pub openai: Option<ProviderConfig>,
}

/// Configuration for a single LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API key. If absent, falls back to environment variable.
    pub api_key: Option<String>,
    /// Default model name.
    pub model: Option<String>,
    /// Custom base URL (e.g. for OpenAI-compatible endpoints).
    pub base_url: Option<String>,
}

/// MCP server definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Display name (e.g. "bash", "browser", "filesystem").
    pub name: String,
    /// Command to run (e.g. "mcp-server-bash").
    pub command: String,
    /// Arguments to pass to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Transport type (currently only "stdio" supported).
    #[serde(default = "default_transport")]
    pub transport: String,
    /// Environment variables for the MCP server process.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

fn default_transport() -> String {
    "stdio".to_string()
}

/// General agent settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Maximum tool-call iterations per prompt (default 25).
    #[serde(default = "default_max_iterations")]
    pub max_tool_iterations: usize,
    /// Retry policy settings.
    #[serde(default)]
    pub retry: RetryConfig,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_tool_iterations: default_max_iterations(),
            retry: RetryConfig::default(),
        }
    }
}

fn default_max_iterations() -> usize {
    25
}

/// Retry policy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_base_delay")]
    pub base_delay_ms: u64,
    #[serde(default = "default_max_delay")]
    pub max_delay_ms: u64,
    #[serde(default = "default_multiplier")]
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            base_delay_ms: default_base_delay(),
            max_delay_ms: default_max_delay(),
            backoff_multiplier: default_multiplier(),
        }
    }
}

fn default_max_retries() -> u32 { 3 }
fn default_base_delay() -> u64 { 1000 }
fn default_max_delay() -> u64 { 30000 }
fn default_multiplier() -> f64 { 2.0 }

/// System prompt settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    /// Custom default system prompt text.
    pub default: Option<String>,
    /// Per-mode prompts.
    #[serde(default)]
    pub modes: HashMap<String, String>,
}

/// Bridge / serve mode settings.
///
/// Configured under `[serve]` in `config.toml` or a workspace's `config.toml`.
/// CLI flags passed to `aptove run` take precedence over these values.
///
/// Transport selection is now driven by `common.toml` (see `CommonConfig`);
/// the `transport` field previously found here has been removed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeConfig {
    /// TCP port for the WebSocket bridge server (default 8765).
    #[serde(default = "default_serve_port")]
    pub port: u16,
    /// Bind address (default `"0.0.0.0"`).
    #[serde(default = "default_bind_addr")]
    pub bind_addr: String,
    /// Enable TLS with a self-signed certificate (default true).
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Optional bearer token clients must supply to connect.
    pub auth_token: Option<String>,
    /// Keep a warm agent pool for faster session creation (default false).
    #[serde(default)]
    pub keep_alive: bool,
    /// Override the IP/hostname advertised in the QR code pairing URL.
    /// Useful when running inside Docker or Apple Native containers where
    /// the auto-detected IP is an internal virtual address unreachable from
    /// mobile devices. Set to the host machine's real LAN IP (e.g. "192.168.1.50").
    pub advertise_addr: Option<String>,
}

fn default_serve_port() -> u16 { 8765 }
fn default_bind_addr() -> String { "0.0.0.0".to_string() }
fn default_tls() -> bool { true }

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            port: default_serve_port(),
            bind_addr: default_bind_addr(),
            tls: default_tls(),
            auth_token: None,
            keep_alive: false,
            advertise_addr: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

impl AgentConfig {
    /// Platform-specific data directory for Aptove.
    ///
    /// - macOS: `~/Library/Application Support/Aptove/`
    /// - Linux: `~/.local/share/Aptove/`
    /// - Windows: `%LOCALAPPDATA%\Aptove\`
    pub fn data_dir() -> Result<PathBuf> {
        let base = dirs::data_local_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?;
        Ok(base.join("Aptove"))
    }

    /// Load config from the default location.
    ///
    /// Checks `<data_dir>/Aptove/config.toml` first, then falls back to
    /// the legacy location `<config_dir>/Aptove/config.toml`.
    pub fn load_default() -> Result<Self> {
        // Try new data_dir location first
        if let Ok(data_path) = Self::data_dir().map(|d| d.join("config.toml")) {
            if data_path.exists() {
                return Self::load_from(&data_path);
            }
        }

        // Fall back to legacy config_dir location
        let legacy_path = Self::legacy_config_path()?;
        if legacy_path.exists() {
            info!("loading config from legacy location: {}", legacy_path.display());
            return Self::load_from(&legacy_path);
        }

        let primary = Self::data_dir()
            .map(|d| d.join("config.toml"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "<unknown>".into());
        tracing::warn!(
            "no config file found (searched {} and {}). \
             Using defaults. Run `aptove config init` to create one, \
             or set API key env vars (ANTHROPIC_API_KEY, GOOGLE_API_KEY, OPENAI_API_KEY).",
            primary,
            legacy_path.display(),
        );
        Ok(Self::default())
    }

    /// Load config from a specific path.
    pub fn load_from(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("failed to parse config: {}", path.display()))?;
        info!(path = %path.display(), provider = %config.provider, "loaded config");
        Ok(config)
    }

    /// Load a global config and optionally merge with a workspace config overlay.
    ///
    /// Workspace overlays may only set `provider`, `providers.*`, `mcp_servers`,
    /// `agent`, `system_prompt`, and `serve`. Keys `identity`, `transports`, and
    /// `state` are silently stripped from the workspace overlay before merging.
    pub fn load_with_workspace(workspace_config_path: Option<&Path>) -> Result<Self> {
        let base = Self::load_default()?;
        if let Some(ws_path) = workspace_config_path {
            if ws_path.exists() {
                let base_str = toml::to_string(&base)?;
                let ws_str = std::fs::read_to_string(ws_path)
                    .with_context(|| format!("failed to read workspace config: {}", ws_path.display()))?;
                return merge_toml_configs_workspace(&base_str, &ws_str);
            }
        }
        Ok(base)
    }

    /// Default config file path (new location in data dir).
    pub fn default_path() -> Result<PathBuf> {
        Ok(Self::data_dir()?.join("config.toml"))
    }

    /// Legacy config file path (`~/.config/Aptove/config.toml`).
    pub fn legacy_config_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
        Ok(dir.join("Aptove").join("config.toml"))
    }

    /// Resolve the API key for a provider, checking config and then env vars.
    pub fn resolve_api_key(&self, provider_name: &str) -> Option<String> {
        // Check config first
        let config_key = match provider_name {
            "claude" => self.providers.claude.as_ref().and_then(|p| p.api_key.clone()),
            "gemini" => self.providers.gemini.as_ref().and_then(|p| p.api_key.clone()),
            "openai" => self.providers.openai.as_ref().and_then(|p| p.api_key.clone()),
            _ => None,
        };

        if config_key.is_some() {
            return config_key;
        }

        // Fall back to environment variables
        let env_var = match provider_name {
            "claude" => "ANTHROPIC_API_KEY",
            "gemini" => "GOOGLE_API_KEY",
            "openai" => "OPENAI_API_KEY",
            _ => return None,
        };

        std::env::var(env_var).ok()
    }

    /// Get the model name for a provider.
    pub fn model_for_provider(&self, provider_name: &str) -> String {
        let configured = match provider_name {
            "claude" => self.providers.claude.as_ref().and_then(|p| p.model.clone()),
            "gemini" => self.providers.gemini.as_ref().and_then(|p| p.model.clone()),
            "openai" => self.providers.openai.as_ref().and_then(|p| p.model.clone()),
            _ => None,
        };

        configured.unwrap_or_else(|| match provider_name {
            "claude" => "claude-sonnet-4-20250514".to_string(),
            "gemini" => "gemini-2.5-pro".to_string(),
            "openai" => "gpt-4o".to_string(),
            _ => "unknown".to_string(),
        })
    }

    /// Ensure `identity.agent_id` and `identity.auth_token` are populated.
    ///
    /// Generates and saves them if either is empty. Call this once at startup
    /// before building the bridge server.
    pub fn ensure_identity(&mut self) {
        let mut changed = false;
        if self.identity.agent_id.is_empty() {
            self.identity.agent_id = uuid::Uuid::new_v4().to_string();
            changed = true;
        }
        if self.identity.auth_token.is_empty() {
            self.identity.auth_token = generate_token();
            changed = true;
        }
        if changed {
            if let Err(e) = self.save() {
                tracing::warn!("failed to persist generated identity: {}", e);
            }
        }
    }

    /// Atomically write the full config (including current state) to the default path.
    ///
    /// Uses a `.config.toml.tmp` temp file + rename to prevent corruption on crash.
    pub fn save(&self) -> Result<()> {
        let path = Self::default_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("toml.tmp");
        let content = toml::to_string_pretty(self)
            .context("failed to serialize config")?;
        std::fs::write(&tmp, content)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("failed to rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Validate the config on startup. Returns a list of warnings.
    pub fn validate(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();

        // Check that the active provider has an API key
        if self.resolve_api_key(&self.provider).is_none() {
            let env_var = match self.provider.as_str() {
                "claude" => "ANTHROPIC_API_KEY",
                "gemini" => "GOOGLE_API_KEY",
                "openai" => "OPENAI_API_KEY",
                other => {
                    bail!("unknown provider: '{}'. Expected: claude, gemini, or openai", other);
                }
            };
            let config_path = Self::default_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".into());
            bail!(
                "No API key for provider '{}'. \
                 Set the {} environment variable, \
                 or add api_key under [providers.{}] in {}.\n\
                 Run `aptove config init` to create a sample config file.",
                self.provider,
                env_var,
                self.provider,
                config_path,
            );
        }

        // Check MCP server commands exist on PATH
        for server in &self.mcp_servers {
            if which::which(&server.command).is_err() {
                warnings.push(format!(
                    "MCP server '{}': command '{}' not found on PATH",
                    server.name, server.command
                ));
            }
        }

        Ok(warnings)
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            providers: ProvidersConfig::default(),
            mcp_servers: Vec::new(),
            agent: AgentSettings::default(),
            system_prompt: SystemPromptConfig::default(),
            serve: ServeConfig::default(),
            identity: IdentityConfig::default(),
            transports: TransportsConfig::default(),
            state: AppStateConfig::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Config generation (for `config init`)
// ---------------------------------------------------------------------------

/// Generate a sample config TOML string.
pub fn sample_config() -> String {
    r#"# Aptove Agent Configuration

# Active LLM provider: "claude", "gemini", or "openai"
provider = "claude"

[providers.claude]
# api_key = "sk-ant-..."  # Or set ANTHROPIC_API_KEY env var
model = "claude-sonnet-4-20250514"

[providers.gemini]
# api_key = "..."  # Or set GOOGLE_API_KEY env var
model = "gemini-2.5-pro"

[providers.openai]
# api_key = "sk-..."  # Or set OPENAI_API_KEY env var
model = "gpt-4o"
# base_url = "https://api.openai.com"  # For compatible endpoints

# MCP Servers — tools the agent can use
[[mcp_servers]]
name = "bash"
command = "mcp-server-bash"
args = []

# [[mcp_servers]]
# name = "filesystem"
# command = "mcp-server-filesystem"
# args = ["/path/to/workspace"]

# [[mcp_servers]]
# name = "browser"
# command = "mcp-server-browser"
# args = ["--headless"]

[agent]
max_tool_iterations = 25

[agent.retry]
max_retries = 3
base_delay_ms = 1000
max_delay_ms = 30000
backoff_multiplier = 2.0

# [system_prompt]
# default = "You are a helpful AI coding assistant."
# [system_prompt.modes]
# planning = "You are in planning mode. Think step by step."
# reviewing = "You are reviewing code. Focus on bugs and improvements."

# Serve / bridge settings — can be overridden per-workspace in
# <data_dir>/workspaces/<uuid>/config.toml
# [serve]
# bind_addr = "0.0.0.0"
# keep_alive = false
# advertise_addr = "192.168.1.50"  # override auto-detected LAN IP (containers with -p)

# ─────────────────────────────────────────────────────────────────────────────
# Identity — generated automatically on first launch. Do not share auth_token.
# OWNED by this file; workspace configs cannot override these.
# ─────────────────────────────────────────────────────────────────────────────
[identity]
# agent_id = "uuid-v4"      # Auto-generated; identifies this agent instance
# auth_token = "hex-token"  # Auto-generated; clients must supply this to connect

# ─────────────────────────────────────────────────────────────────────────────
# Network transports — configure which connection methods are active.
# OWNED by this file; workspace configs cannot override these.
# ─────────────────────────────────────────────────────────────────────────────

[transports.local]
enabled = true
port = 8765
tls = true

# [transports.tailscale-serve]
# enabled = false
# port = 8766

# [transports.tailscale-ip]
# enabled = false
# port = 8765

# [transports.cloudflare]
# enabled = false
# hostname = ""       # e.g. "https://agent.example.com"
# tunnel_id = ""
# account_id = ""
# tunnel_secret = ""

# ─────────────────────────────────────────────────────────────────────────────
# Persisted TUI state — written automatically on exit. Do not edit by hand.
# OWNED by this file; workspace configs cannot override these.
# ─────────────────────────────────────────────────────────────────────────────
[state]
last_tab = "Setup"
tailscale_enabled = false
cloudflare_enabled = false
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// Config merging
// ---------------------------------------------------------------------------

/// Keys that are owned by the global config and cannot be set in workspace overlays.
const WORKSPACE_BLOCKED_KEYS: &[&str] = &["identity", "transports", "state"];

/// Merge a base TOML config with a workspace overlay.
///
/// All fields in the overlay override base config fields. Use this for
/// non-workspace merges (e.g. unit tests, programmatic merges).
pub fn merge_toml_configs(base_toml: &str, overlay_toml: &str) -> Result<AgentConfig> {
    let mut base_val: toml::Value = toml::from_str(base_toml)
        .context("failed to parse base config TOML")?;
    let overlay_val: toml::Value = toml::from_str(overlay_toml)
        .context("failed to parse overlay config TOML")?;
    merge_toml_values(&mut base_val, &overlay_val);
    let config: AgentConfig = base_val.try_into()
        .context("merged config is not a valid AgentConfig")?;
    Ok(config)
}

/// Merge a base TOML config with a workspace overlay, stripping owned keys.
///
/// `identity`, `transports`, and `state` are silently removed from the overlay
/// before merging so workspace configs cannot override them.
fn merge_toml_configs_workspace(base_toml: &str, overlay_toml: &str) -> Result<AgentConfig> {
    let mut base_val: toml::Value = toml::from_str(base_toml)
        .context("failed to parse base config TOML")?;
    let mut overlay_val: toml::Value = toml::from_str(overlay_toml)
        .context("failed to parse overlay config TOML")?;

    // Strip owned keys from the workspace overlay
    if let Some(table) = overlay_val.as_table_mut() {
        for key in WORKSPACE_BLOCKED_KEYS {
            table.remove(*key);
        }
    }

    merge_toml_values(&mut base_val, &overlay_val);
    let config: AgentConfig = base_val.try_into()
        .context("merged config is not a valid AgentConfig")?;
    Ok(config)
}

fn merge_toml_values(base: &mut toml::Value, overlay: &toml::Value) {
    if base.is_table() && overlay.is_table() {
        let overlay_table = overlay.as_table().unwrap().clone();
        let base_table = base.as_table_mut().unwrap();
        for (key, value) in overlay_table {
            if let Some(base_value) = base_table.get_mut(&key) {
                merge_toml_values(base_value, &value);
            } else {
                base_table.insert(key, value);
            }
        }
    } else {
        *base = overlay.clone();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml_str = r#"
            provider = "claude"
        "#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.provider, "claude");
        assert!(config.mcp_servers.is_empty());
    }

    #[test]
    fn parse_full_config() {
        let toml_str = r#"
            provider = "openai"

            [providers.openai]
            api_key = "sk-test"
            model = "gpt-4o"
            base_url = "https://api.openai.com"

            [[mcp_servers]]
            name = "bash"
            command = "mcp-server-bash"
            args = []

            [agent]
            max_tool_iterations = 10

            [agent.retry]
            max_retries = 5
        "#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.provider, "openai");
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.agent.max_tool_iterations, 10);
        assert_eq!(config.agent.retry.max_retries, 5);
    }

    #[test]
    fn resolve_api_key_from_config() {
        let config = AgentConfig {
            providers: ProvidersConfig {
                claude: Some(ProviderConfig {
                    api_key: Some("sk-from-config".to_string()),
                    model: None,
                    base_url: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            config.resolve_api_key("claude"),
            Some("sk-from-config".to_string())
        );
    }

    #[test]
    fn default_model_names() {
        let config = AgentConfig::default();
        assert_eq!(config.model_for_provider("claude"), "claude-sonnet-4-20250514");
        assert_eq!(config.model_for_provider("gemini"), "gemini-2.5-pro");
        assert_eq!(config.model_for_provider("openai"), "gpt-4o");
    }

    #[test]
    fn sample_config_parses() {
        let sample = sample_config();
        // Should parse without error (comments are valid TOML)
        let _config: AgentConfig = toml::from_str(&sample).unwrap();
    }

    #[test]
    fn merge_overrides_provider() {
        let base = r#"provider = "claude""#;
        let overlay = r#"provider = "gemini""#;
        let merged = merge_toml_configs(base, overlay).unwrap();
        assert_eq!(merged.provider, "gemini");
    }

    #[test]
    fn merge_preserves_base_when_overlay_empty() {
        let base = r#"
            provider = "claude"
            [agent]
            max_tool_iterations = 10
        "#;
        let overlay = "";
        let merged = merge_toml_configs(base, overlay).unwrap();
        assert_eq!(merged.provider, "claude");
        assert_eq!(merged.agent.max_tool_iterations, 10);
    }

    #[test]
    fn merge_nested_tables() {
        let base = r#"
            provider = "claude"
            [providers.claude]
            model = "sonnet"
        "#;
        let overlay = r#"
            [providers.claude]
            model = "opus"
        "#;
        let merged = merge_toml_configs(base, overlay).unwrap();
        assert_eq!(
            merged.providers.claude.as_ref().unwrap().model.as_deref(),
            Some("opus")
        );
    }

    #[test]
    fn merge_adds_new_fields() {
        let base = r#"provider = "claude""#;
        let overlay = r#"
            [[mcp_servers]]
            name = "bash"
            command = "mcp-server-bash"
        "#;
        let merged = merge_toml_configs(base, overlay).unwrap();
        assert_eq!(merged.provider, "claude");
        assert_eq!(merged.mcp_servers.len(), 1);
        assert_eq!(merged.mcp_servers[0].name, "bash");
    }

    #[test]
    fn data_dir_is_valid() {
        let dir = AgentConfig::data_dir();
        assert!(dir.is_ok());
        let path = dir.unwrap();
        assert!(path.to_string_lossy().contains("Aptove"));
    }

    #[test]
    fn parse_identity_and_transports() {
        let toml_str = r#"
            provider = "claude"

            [identity]
            agent_id = "test-uuid"
            auth_token = "tok_secret"

            [transports.local]
            enabled = true
            port = 8765
            tls = true

            [transports.cloudflare]
            enabled = false
            hostname = "https://agent.example.com"
        "#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.identity.agent_id, "test-uuid");
        assert_eq!(config.identity.auth_token, "tok_secret");
        assert!(config.transports.local.enabled);
        assert_eq!(config.transports.local.port, 8765);
        assert!(!config.transports.cloudflare.enabled);
        assert_eq!(config.transports.cloudflare.hostname, "https://agent.example.com");
    }

    #[test]
    fn parse_state_section() {
        let toml_str = r#"
            provider = "claude"

            [state]
            last_tab = "Chat"
            tailscale_enabled = true
            cloudflare_enabled = false
        "#;
        let config: AgentConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.state.last_tab, "Chat");
        assert!(config.state.tailscale_enabled);
        assert!(!config.state.cloudflare_enabled);
    }

    #[test]
    fn default_transports_local_enabled() {
        let config = AgentConfig::default();
        assert!(config.transports.local.enabled);
        assert_eq!(config.transports.local.port, 8765);
        assert!(!config.transports.tailscale_serve.enabled);
        assert!(!config.transports.tailscale_ip.enabled);
        assert!(!config.transports.cloudflare.enabled);
    }

    #[test]
    fn enabled_transport_names() {
        let mut config = AgentConfig::default();
        config.transports.local.enabled = true;
        config.transports.tailscale_serve.enabled = true;
        config.transports.cloudflare.enabled = false;
        let names = config.transports.enabled_names();
        assert_eq!(names, vec!["local", "tailscale-serve"]);
    }

    #[test]
    fn workspace_cannot_override_identity() {
        let base = r#"
            provider = "claude"
            [identity]
            agent_id = "original-id"
            auth_token = "original-token"
        "#;
        let overlay = r#"
            [identity]
            agent_id = "hacked-id"
            auth_token = "hacked-token"
        "#;
        let merged = merge_toml_configs_workspace(base, overlay).unwrap();
        assert_eq!(merged.identity.agent_id, "original-id");
        assert_eq!(merged.identity.auth_token, "original-token");
    }

    #[test]
    fn workspace_cannot_override_transports() {
        let base = r#"
            provider = "claude"
            [transports.local]
            enabled = true
            port = 8765
        "#;
        let overlay = r#"
            [transports.local]
            enabled = false
            port = 9999
        "#;
        let merged = merge_toml_configs_workspace(base, overlay).unwrap();
        assert!(merged.transports.local.enabled);
        assert_eq!(merged.transports.local.port, 8765);
    }

    #[test]
    fn workspace_cannot_override_state() {
        let base = r#"
            provider = "claude"
            [state]
            last_tab = "Jobs"
            tailscale_enabled = true
        "#;
        let overlay = r#"
            [state]
            last_tab = "Setup"
            tailscale_enabled = false
        "#;
        let merged = merge_toml_configs_workspace(base, overlay).unwrap();
        assert_eq!(merged.state.last_tab, "Jobs");
        assert!(merged.state.tailscale_enabled);
    }

    #[test]
    fn workspace_can_override_provider() {
        let base = r#"provider = "claude""#;
        let overlay = r#"provider = "gemini""#;
        let merged = merge_toml_configs_workspace(base, overlay).unwrap();
        assert_eq!(merged.provider, "gemini");
    }

    #[test]
    fn sample_config_parses_with_new_sections() {
        let sample = sample_config();
        let config: AgentConfig = toml::from_str(&sample).unwrap();
        // New sections should parse without error
        assert!(config.transports.local.enabled);
        assert_eq!(config.state.last_tab, "Setup");
    }
}
