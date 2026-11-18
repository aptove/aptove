//! CommonConfig — single source of truth for agent identity and transport settings.
//!
//! Stored as `common.toml` in the bridge config directory.

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Global custom config directory for CommonConfig (set via --config-dir).
static COMMON_CUSTOM_CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set a custom config directory (call before any config operations).
pub fn set_config_dir(path: PathBuf) {
    COMMON_CUSTOM_CONFIG_DIR.set(path).ok();
}

/// A slash command advertised to connected clients via `available_commands_update`.
///
/// Define these in `common.toml` for agents that don't send `available_commands_update`
/// themselves (e.g. Copilot CLI, Goose).
///
/// Example `common.toml` entry:
/// ```toml
/// [[slash_commands]]
/// name        = "fix"
/// description = "Fix the selected code"
///
/// [[slash_commands]]
/// name        = "explain"
/// description = "Explain the code, optionally with a focus"
/// input_hint  = "what to focus on"
/// ```
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SlashCommandConfig {
    /// Command name without the leading `/` (e.g. `"fix"`).
    pub name: String,
    /// Human-readable description shown in the picker.
    pub description: String,
    /// If set, the command accepts free-text input; this string is shown as
    /// the placeholder hint in the text field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

/// Which push provider the user has selected.
///
/// `None` (the `Option` default) means the user has not yet made a choice.
/// After any explicit selection — including disabling push — this is `Some(...)`.
/// The underlying `push_relay` credentials are preserved regardless of this
/// selection so the user can re-enable push without re-entering them.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PushSelection {
    /// Push notifications are explicitly disabled.
    None,
    /// Aptove hosted push service.
    Aptove,
    /// Self-managed / custom push service.
    Custom,
}

/// Push relay configuration for sending background notifications.
///
/// All four fields are required — push is silently disabled if the section is
/// absent or any field is empty.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct PushRelayConfig {
    /// Base URL of the push relay service (e.g. "https://push.aptove.com").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Base URL of the JWT token service (e.g. "https://token.aptove.com").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token_url: String,
    /// OAuth2 client_id issued by the token service.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_id: String,
    /// OAuth2 client_secret issued by the token service.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
}

/// Stable agent identity and multi-transport settings. Stored as `common.toml`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CommonConfig {
    /// Stable UUID that identifies this agent across all transports.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,

    /// Bearer token required for WebSocket connections.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth_token: String,

    /// Per-transport configuration, keyed by transport name
    /// (e.g., `"local"`, `"cloudflare"`, `"tailscale-serve"`).
    #[serde(default)]
    pub transports: HashMap<String, TransportConfig>,

    /// Slash commands to advertise to clients via `available_commands_update`.
    /// Used for agents that don't send this notification themselves.
    /// The bridge injects the notification after every session/new or session/load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slash_commands: Vec<SlashCommandConfig>,

    /// Aptove hosted push service credentials.
    /// Stored independently from `custom_push_relay` so configuring one
    /// provider never overwrites the other's credentials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aptove_push_relay: Option<PushRelayConfig>,

    /// Self-managed / custom push service credentials.
    /// Stored independently from `aptove_push_relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_push_relay: Option<PushRelayConfig>,

    /// Which push provider the user has explicitly selected.
    /// `None` means no choice has been made yet (triggers the first-run push menu).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_selection: Option<PushSelection>,

    /// Agent command to launch (e.g., "copilot --acp"). Stored here so the
    /// wizard only asks once; previously it was a CLI flag on `bridge run`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_command: Option<String>,

    /// TCP address to bind the WebSocket server (default: "0.0.0.0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_address: Option<String>,

    /// Override the advertised LAN address in the QR / pairing URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise_addr: Option<String>,

    /// Prevent idle and system sleep while the bridge is running (default: false).
    /// Display sleep is not blocked — the screen can still lock after the normal
    /// idle timeout. Enable via the first-run wizard or `/keep-alive` in the TUI.
    #[serde(default = "keep_alive_default")]
    pub keep_alive: bool,

    /// Set to true once the user has been shown the keep-alive prompt during
    /// first-run setup. Used to avoid re-prompting on subsequent startups.
    #[serde(default, skip_serializing_if = "is_false")]
    pub keep_alive_prompted: bool,

    /// Minimum log level shown in the TUI (ERROR / WARN / INFO / DEBUG / TRACE).
    #[serde(default = "log_level_default")]
    pub log_level: String,

    /// Maximum size of each log file in MB before rotation (default: 10).
    #[serde(default = "log_file_max_mb_default")]
    pub log_file_max_mb: u64,

    /// Number of rotated log files to keep in `~/.aptove/projects/<cwd>/logs/` (default: 5).
    #[serde(default = "log_file_count_default")]
    pub log_file_count: usize,

    /// Maximum size of MEMORY.md in KB. Bridge warns at 80% and blocks new
    /// appends (notifying the mobile client) once this limit is reached.
    /// Default: 1024 KB (1 MB).
    #[serde(default = "memory_max_kb_default")]
    pub memory_max_kb: u64,

    /// Maximum WebSocket message size in MB. Increase if users send many large
    /// images in a single prompt. Default: 50 MB.
    #[serde(default = "ws_max_message_mb_default")]
    pub ws_max_message_mb: u64,

    /// Transport name used in the last session. Auto-selected on subsequent
    /// startups to skip the transport chooser when the selection is still valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transport: Option<String>,

    /// User-visible name for this agent instance, shown in push notification titles.
    /// Defaults to the folder name where aptove is started. Can be updated by the
    /// mobile app via `bridge/setAgentName`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,

    /// Include raw message content (prompts, agent replies) in log files.
    ///
    /// **Default: `false`.** When `false`, log lines record only byte counts
    /// and method names — conversation text never reaches the disk.  Set to
    /// `true` only for short-lived debugging sessions; anyone with read access
    /// to the log directory can then see conversation content.
    #[serde(default = "log_message_content_default", skip_serializing_if = "is_false")]
    pub log_message_content: bool,

    /// Use a generic `"Aptove"` title in push notifications instead of `agent_name`
    /// (default: `false`).
    ///
    /// When `false` (the default), the notification title is the value of
    /// `agent_name` — typically the working-directory folder name. Set to `true`
    /// for privacy: the title is always `"Aptove"` and `agent_name` is omitted
    /// from the notification payload entirely, so it never reaches the push relay,
    /// Apple APNs, or Google FCM.
    #[serde(default = "push_generic_title_default")]
    pub push_generic_title: bool,
}

fn keep_alive_default() -> bool { false }
fn log_level_default() -> String { "WARN".to_string() }
fn log_file_max_mb_default() -> u64 { 10 }
fn log_file_count_default() -> usize { 5 }
fn memory_max_kb_default() -> u64 { 1024 }
fn ws_max_message_mb_default() -> u64 { 50 }
fn log_message_content_default() -> bool { false }
fn push_generic_title_default() -> bool { false }
fn is_false(v: &bool) -> bool { !v }

/// Configuration for a single transport.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct TransportConfig {
    /// Whether this transport is active.
    #[serde(default)]
    pub enabled: bool,

    /// TCP port to bind (local transport).
    pub port: Option<u16>,

    /// Enable TLS on this transport (default: true for local).
    pub tls: Option<bool>,

    // ---- Cloudflare Zero Trust fields (transport name: "cloudflare") ----
    pub hostname: Option<String>,
    pub tunnel_id: Option<String>,
    pub tunnel_secret: Option<String>,
    pub account_id: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub domain: Option<String>,
    pub subdomain: Option<String>,
}

impl Default for CommonConfig {
    fn default() -> Self {
        // No transports pre-enabled: the setup wizard will ask the user to
        // choose one on first run (or any time no transport is configured).
        Self {
            agent_id: String::new(),
            auth_token: String::new(),
            transports: HashMap::new(),
            slash_commands: Vec::new(),
            aptove_push_relay: None,
            custom_push_relay: None,
            push_selection: None,
            agent_command: None,
            bind_address: None,
            advertise_addr: None,
            keep_alive: false,
            keep_alive_prompted: false,
            log_level: "WARN".to_string(),
            log_file_max_mb: 10,
            log_file_count: 5,
            memory_max_kb: 1024,
            ws_max_message_mb: 50,
            last_transport: None,
            agent_name: None,
            log_message_content: false,
            push_generic_title: false,
        }
    }
}

impl CommonConfig {
    /// Path to the `common.toml` file (default config dir).
    pub fn config_path() -> PathBuf {
        Self::config_dir().join("common.toml")
    }

    /// Config directory (system default or custom override).
    ///
    /// Default location is keyed by the current working directory so each
    /// project gets an isolated config, but nothing is written into the
    /// project folder itself:
    ///
    /// - macOS / Linux: `~/.aptove/projects/<cwd_without_leading_slash>/`
    ///   e.g. `~/.aptove/projects/Users/alice/code/my-project/`
    /// - Windows: `%APPDATA%\Aptove\projects\<cwd_without_drive_prefix>\`
    ///
    /// Pass `--config-dir` (calls `set_config_dir`) to override.
    pub fn config_dir() -> PathBuf {
        let dir = if let Some(custom) = COMMON_CUSTOM_CONFIG_DIR.get() {
            custom.clone()
        } else {
            #[cfg(windows)]
            let base = BaseDirs::new()
                .map(|b| b.data_dir().to_path_buf())
                .unwrap_or_else(|| PathBuf::from("C:\\Users\\Default\\AppData\\Roaming"))
                .join("Aptove");
            #[cfg(not(windows))]
            let base = BaseDirs::new()
                .map(|b| b.home_dir().join(".aptove"))
                .unwrap_or_else(|| PathBuf::from("/tmp/.aptove"));

            let cwd = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."));
            // Strip leading root dir (`/`) or Windows drive prefix (`C:\`) so
            // the path can be joined cleanly under the base directory.
            let cwd_rel: PathBuf = cwd.components()
                .filter(|c| !matches!(c,
                    std::path::Component::RootDir | std::path::Component::Prefix(_)))
                .collect();
            base.join("projects").join(cwd_rel)
        };
        fs::create_dir_all(&dir).ok();
        dir
    }

    /// Load from `common.toml` at the default location, or return defaults.
    pub fn load() -> Result<Self> {
        Self::load_from_dir(&Self::config_dir())
    }

    /// Load from `common.toml` in a specific directory, or return defaults.
    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let path = dir.join("common.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {:?}", path))?;
        let config: Self = toml::from_str(&text)
            .with_context(|| format!("Failed to parse {:?}", path))?;
        Ok(config)
    }

    /// Save to `common.toml` with 0600 permissions (default config dir).
    pub fn save(&self) -> Result<()> {
        self.save_to_dir(&Self::config_dir())
    }

    /// Save to `common.toml` in a specific directory with 0600 permissions.
    ///
    /// Uses an atomic write (write to `.tmp`, restrict permissions, rename) so
    /// the final file is never world-readable, even for a brief window.
    pub fn save_to_dir(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join("common.toml");
        let tmp_path = dir.join("common.toml.tmp");
        let text = toml::to_string_pretty(self).context("Failed to serialize CommonConfig")?;
        fs::write(&tmp_path, &text).with_context(|| format!("Failed to write {:?}", tmp_path))?;
        crate::file_permissions::restrict_file_to_owner(&tmp_path)
            .with_context(|| format!("Failed to set permissions on {:?}", tmp_path))?;
        fs::rename(&tmp_path, &path)
            .with_context(|| format!("Failed to rename {:?} to {:?}", tmp_path, path))?;
        Ok(())
    }

    /// Returns the active push relay config based on `push_selection`.
    ///
    /// Returns `None` when push is disabled, not yet configured, or the
    /// corresponding credentials section is absent.
    pub fn effective_push_relay(&self) -> Option<&PushRelayConfig> {
        match &self.push_selection {
            Some(PushSelection::Aptove) => self.aptove_push_relay.as_ref(),
            Some(PushSelection::Custom) => self.custom_push_relay.as_ref(),
            _ => None,
        }
    }

    /// Generate a UUID v4 `agent_id` if one is not already set.
    pub fn ensure_agent_id(&mut self) {
        if self.agent_id.is_empty() {
            self.agent_id = uuid::Uuid::new_v4().to_string();
        }
    }

    /// Generate a random URL-safe authentication token (32 random bytes, base64).
    pub fn generate_auth_token() -> String {
        use base64::{engine::general_purpose, Engine as _};
        let bytes: Vec<u8> = (0..32).map(|_| rand::random::<u8>()).collect();
        general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Ensure `auth_token` is populated, generating one if needed.
    pub fn ensure_auth_token(&mut self) {
        if self.auth_token.is_empty() {
            self.auth_token = Self::generate_auth_token();
        }
    }

    /// Returns all enabled transports, sorted by name for deterministic ordering.
    pub fn enabled_transports(&self) -> Vec<(&str, &TransportConfig)> {
        let mut result: Vec<_> = self
            .transports
            .iter()
            .filter(|(_, t)| t.enabled)
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        result.sort_by_key(|(k, _)| *k);
        result
    }

    /// Build a static connection JSON payload for a QR code.
    ///
    /// Includes `agentId`, `url`, `protocol`, `version`, `authToken`, and
    /// Cloudflare credentials if present in the transport config.
    pub fn to_connection_json(&self, hostname: &str, transport_name: &str, cwd: &str) -> Result<String> {
        use serde_json::{Map, Value};
        let transport = self.transports.get(transport_name);
        let mut map = Map::new();
        if !self.agent_id.is_empty() {
            map.insert("agentId".to_string(), Value::String(self.agent_id.clone()));
        }
        map.insert("url".to_string(), Value::String(hostname.to_string()));
        map.insert("protocol".to_string(), Value::String("acp".to_string()));
        map.insert("version".to_string(), Value::String("1.0".to_string()));
        map.insert("cwd".to_string(), Value::String(cwd.to_string()));
        if !self.auth_token.is_empty() {
            map.insert(
                "authToken".to_string(),
                Value::String(self.auth_token.clone()),
            );
        }
        if let Some(t) = transport {
            if let Some(ref id) = t.client_id {
                if !id.is_empty() {
                    map.insert("clientId".to_string(), Value::String(id.clone()));
                }
            }
            if let Some(ref secret) = t.client_secret {
                if !secret.is_empty() {
                    map.insert("clientSecret".to_string(), Value::String(secret.clone()));
                }
            }
        }
        serde_json::to_string(&Value::Object(map)).context("Failed to serialize connection info")
    }

    /// Returns the root directory that contains all per-project config dirs.
    ///
    /// - macOS / Linux: `~/.aptove/projects/`
    /// - Windows: `%APPDATA%\Aptove\projects\`
    pub fn projects_root() -> Option<PathBuf> {
        #[cfg(windows)]
        { BaseDirs::new().map(|b| b.data_dir().join("Aptove").join("projects")) }
        #[cfg(not(windows))]
        { BaseDirs::new().map(|b| b.home_dir().join(".aptove").join("projects")) }
    }

    /// Returns the set of TCP ports already claimed by `[transports.local]`
    /// in every sibling project config under `projects_root()`.
    ///
    /// Used to pick a unique port when first enabling Local transport so that
    /// two projects never start with the same configured port, even when only
    /// one of them is currently running.
    pub fn claimed_local_ports() -> HashSet<u16> {
        let current = Self::config_dir();
        let Some(root) = Self::projects_root() else { return HashSet::new(); };
        let mut ports = HashSet::new();
        collect_claimed_ports(&root, &current, &mut ports);
        ports
    }
}

/// Recursive helper for `CommonConfig::claimed_local_ports`.
///
/// Walks `dir` looking for `common.toml` files. When one is found, parses
/// it and records the local transport port (if any), then stops recursing
/// into that directory. Directories containing `dir == skip` are skipped.
fn collect_claimed_ports(dir: &Path, skip: &Path, ports: &mut HashSet<u16>) {
    let toml_path = dir.join("common.toml");
    if toml_path.exists() {
        if dir != skip {
            if let Ok(text) = fs::read_to_string(&toml_path) {
                if let Ok(cfg) = toml::from_str::<CommonConfig>(&text) {
                    if let Some(tc) = cfg.transports.get("local") {
                        if let Some(port) = tc.port {
                            ports.insert(port);
                        }
                    }
                }
            }
        }
        return; // Don't recurse inside a project dir
    }
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect_claimed_ports(&entry.path(), skip, ports);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_has_correct_values() {
        let cfg = CommonConfig::default();
        assert!(cfg.transports.is_empty(), "Default config must have no transports");
        assert!(cfg.agent_command.is_none());
        assert_eq!(cfg.log_level, "WARN");
        assert_eq!(cfg.log_file_max_mb, 10);
        assert_eq!(cfg.log_file_count, 5);
        assert!(!cfg.keep_alive);
        assert!(!cfg.keep_alive_prompted);
        assert!(cfg.agent_id.is_empty());
        assert!(cfg.auth_token.is_empty());
        assert!(!cfg.log_message_content, "log_message_content must default to false");
    }

    #[test]
    fn load_from_empty_dir_returns_default() {
        let dir = tempdir().unwrap();
        let cfg = CommonConfig::load_from_dir(dir.path()).unwrap();
        assert!(cfg.agent_id.is_empty(), "Missing file should yield default config");
    }

    #[test]
    fn save_and_reload_roundtrip() {
        let dir = tempdir().unwrap();
        let mut cfg = CommonConfig::default();
        cfg.agent_id = "test-agent-id".to_string();
        cfg.auth_token = "my-token".to_string();
        cfg.agent_command = Some("copilot --acp".to_string());
        cfg.transports.insert("local".to_string(), TransportConfig {
            enabled: true,
            port: Some(8766),
            ..Default::default()
        });

        cfg.save_to_dir(dir.path()).unwrap();

        let reloaded = CommonConfig::load_from_dir(dir.path()).unwrap();
        assert_eq!(reloaded.agent_id, "test-agent-id");
        assert_eq!(reloaded.auth_token, "my-token");
        assert_eq!(reloaded.agent_command.as_deref(), Some("copilot --acp"));
        let local = reloaded.transports.get("local").unwrap();
        assert!(local.enabled);
        assert_eq!(local.port, Some(8766));
    }

    #[test]
    #[cfg(unix)]
    fn save_creates_file_with_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        CommonConfig::default().save_to_dir(dir.path()).unwrap();
        let meta = std::fs::metadata(dir.path().join("common.toml")).unwrap();
        assert_eq!(
            meta.permissions().mode() & 0o777,
            0o600,
            "common.toml must be readable only by owner"
        );
    }

    #[test]
    fn enabled_transports_filters_disabled() {
        let mut cfg = CommonConfig::default();
        cfg.transports.insert("local".to_string(), TransportConfig { enabled: true, ..Default::default() });
        cfg.transports.insert("cloudflare".to_string(), TransportConfig { enabled: false, ..Default::default() });
        let enabled = cfg.enabled_transports();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].0, "local");
    }

    #[test]
    fn enabled_transports_empty_when_all_disabled() {
        let mut cfg = CommonConfig::default();
        cfg.transports.insert("local".to_string(), TransportConfig { enabled: false, ..Default::default() });
        assert!(cfg.enabled_transports().is_empty());
    }

    #[test]
    fn collect_claimed_ports_reads_siblings() {
        let root = tempdir().unwrap();
        let projects = root.path().join("projects");

        // Project A: claims port 8765
        let proj_a = projects.join("project-a");
        std::fs::create_dir_all(&proj_a).unwrap();
        let mut cfg_a = CommonConfig::default();
        cfg_a.transports.insert("local".to_string(), TransportConfig {
            enabled: true, port: Some(8765), ..Default::default()
        });
        cfg_a.save_to_dir(&proj_a).unwrap();

        // Project B: claims port 8766
        let proj_b = projects.join("project-b");
        std::fs::create_dir_all(&proj_b).unwrap();
        let mut cfg_b = CommonConfig::default();
        cfg_b.transports.insert("local".to_string(), TransportConfig {
            enabled: true, port: Some(8766), ..Default::default()
        });
        cfg_b.save_to_dir(&proj_b).unwrap();

        // Project C (current): should see A and B as claimed
        let proj_c = projects.join("project-c");
        std::fs::create_dir_all(&proj_c).unwrap();

        let mut claimed = HashSet::new();
        collect_claimed_ports(&projects, &proj_c, &mut claimed);

        assert!(claimed.contains(&8765), "port 8765 (project-a) should be claimed");
        assert!(claimed.contains(&8766), "port 8766 (project-b) should be claimed");
        assert!(!claimed.contains(&8767), "8767 is unclaimed");
    }

    #[test]
    fn collect_claimed_ports_skips_current_project() {
        let root = tempdir().unwrap();
        let projects = root.path().join("projects");

        // Current project has port 8765
        let current = projects.join("current");
        std::fs::create_dir_all(&current).unwrap();
        let mut cfg = CommonConfig::default();
        cfg.transports.insert("local".to_string(), TransportConfig {
            enabled: true, port: Some(8765), ..Default::default()
        });
        cfg.save_to_dir(&current).unwrap();

        let mut claimed = HashSet::new();
        collect_claimed_ports(&projects, &current, &mut claimed);

        assert!(!claimed.contains(&8765), "current project's own port must not be in claimed set");
    }

    #[test]
    fn generate_auth_token_is_unique() {
        let a = CommonConfig::generate_auth_token();
        let b = CommonConfig::generate_auth_token();
        assert_ne!(a, b, "Two consecutive generate_auth_token() calls must return different values");
        assert!(!a.is_empty());
        assert!(!b.is_empty());
    }

    #[test]
    fn ensure_auth_token_does_not_overwrite_existing() {
        let mut cfg = CommonConfig::default();
        cfg.auth_token = "existing-token".to_string();
        cfg.ensure_auth_token();
        assert_eq!(cfg.auth_token, "existing-token", "ensure_auth_token must not replace a non-empty token");
    }
}
