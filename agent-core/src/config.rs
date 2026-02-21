//! Configuration
//!
//! TOML-based configuration: provider selection, API keys, model defaults,
//! MCP server definitions. Includes startup validation.

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

    /// Bridge / serve mode settings.
    #[serde(default)]
    pub serve: ServeConfig,
}

fn default_provider() -> String {
    "claude".to_string()
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
/// CLI flags passed to `aptove serve` take precedence over these values.
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
    /// Network transport mode (default `"local"`).
    ///
    /// Accepted values: `"local"`, `"cloudflare"`, `"tailscale-serve"`, `"tailscale-ip"`.
    #[serde(default = "default_serve_transport")]
    pub transport: String,
    /// Optional bearer token clients must supply to connect.
    pub auth_token: Option<String>,
    /// Keep a warm agent pool for faster session creation (default false).
    #[serde(default)]
    pub keep_alive: bool,
}

fn default_serve_port() -> u16 { 8765 }
fn default_bind_addr() -> String { "0.0.0.0".to_string() }
fn default_tls() -> bool { true }
fn default_serve_transport() -> String { "local".to_string() }

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            port: default_serve_port(),
            bind_addr: default_bind_addr(),
            tls: default_tls(),
            transport: default_serve_transport(),
            auth_token: None,
            keep_alive: false,
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
    pub fn load_with_workspace(workspace_config_path: Option<&Path>) -> Result<Self> {
        let base = Self::load_default()?;
        if let Some(ws_path) = workspace_config_path {
            if ws_path.exists() {
                let base_str = toml::to_string(&base)?;
                let ws_str = std::fs::read_to_string(ws_path)
                    .with_context(|| format!("failed to read workspace config: {}", ws_path.display()))?;
                return merge_toml_configs(&base_str, &ws_str);
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
        }
    }
}

// ---------------------------------------------------------------------------
// Config generation (for `config init`)
// ---------------------------------------------------------------------------

/// Generate a sample config TOML string.
pub fn sample_config() -> String {
    r#"# Aptove Agent Configuration
# See: https://github.com/agentclientprotocol/rust-sdk

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

# Bridge / serve mode settings (used by `aptove serve`)
# These can be overridden per-workspace in:
#   <data_dir>/workspaces/<uuid>/config.toml
# CLI flags (--port, --tls, --transport, --bind) take precedence over these values.
#
# [serve]
# port = 8765
# bind_addr = "0.0.0.0"
# tls = true
# transport = "local"    # local | cloudflare | tailscale-serve | tailscale-ip
# auth_token = "secret"  # optional bearer token for WebSocket connections
# keep_alive = false
"#
    .to_string()
}

// ---------------------------------------------------------------------------
// Config merging
// ---------------------------------------------------------------------------

/// Merge a base TOML config with a workspace overlay.
///
/// Workspace config fields override base config fields. Nested tables
/// are merged recursively.
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
}
