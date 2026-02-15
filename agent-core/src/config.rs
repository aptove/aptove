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

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

impl AgentConfig {
    /// Load config from the default location:
    /// `~/.config/acp-agent/config.toml`
    pub fn load_default() -> Result<Self> {
        let path = Self::default_path()?;
        if path.exists() {
            Self::load_from(&path)
        } else {
            info!("no config file found at {}, using defaults", path.display());
            Ok(Self::default())
        }
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

    /// Default config file path.
    pub fn default_path() -> Result<PathBuf> {
        let dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("could not determine config directory"))?;
        Ok(dir.join("acp-agent").join("config.toml"))
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
            bail!(
                "No API key for provider '{}'. Set {} environment variable or add api_key under [providers.{}]",
                self.provider,
                env_var,
                self.provider
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
"#
    .to_string()
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
}
