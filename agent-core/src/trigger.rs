//! Webhook Trigger Types and `TriggerStore` Trait
//!
//! Defines the data model for event-driven webhook triggers and the storage
//! trait that backends (filesystem, database, etc.) must implement.
//!
//! The execution engine lives in `agent-scheduler` to keep this crate lean.

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// TriggerStatus
// ---------------------------------------------------------------------------

/// Status of a trigger or individual trigger run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TriggerStatus {
    /// Trigger is active and accepting events.
    Active,
    /// Trigger is disabled.
    Disabled,
    /// Last run completed successfully.
    Success,
    /// Last run encountered an error.
    Error,
}

impl std::fmt::Display for TriggerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerStatus::Active => write!(f, "active"),
            TriggerStatus::Disabled => write!(f, "disabled"),
            TriggerStatus::Success => write!(f, "success"),
            TriggerStatus::Error => write!(f, "error"),
        }
    }
}

// ---------------------------------------------------------------------------
// TriggerDefinition
// ---------------------------------------------------------------------------

/// A complete webhook trigger definition (on-disk / trait boundary type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerDefinition {
    /// UUID for this trigger (set by the store on create).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Prompt template containing `{{payload}}` and other placeholders.
    pub prompt_template: String,
    /// Cryptographic webhook token, prefixed with `wh_`.
    pub token: String,
    /// Whether the trigger is accepting incoming events.
    pub enabled: bool,
    /// Whether to send a push notification on run completion.
    pub notify: bool,
    /// Keep the last N run records (0 = keep all).
    pub max_history: u32,
    /// Maximum events accepted per minute (0 = unlimited).
    pub rate_limit_per_minute: u32,
    /// Optional HMAC-SHA256 secret for request signature verification.
    pub hmac_secret: Option<String>,
    /// Accepted Content-Type values. Empty = accept any content type.
    pub accepted_content_types: Vec<String>,
    /// When the trigger was created.
    pub created_at: DateTime<Utc>,
    /// When the trigger was last modified.
    pub updated_at: DateTime<Utc>,
    /// When the trigger last received an event (None if never fired).
    pub last_event_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// TriggerSummary
// ---------------------------------------------------------------------------

/// Lightweight view of a trigger for list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerSummary {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// The webhook token (used to construct the webhook URL on the client).
    pub token: String,
    pub last_event_at: Option<DateTime<Utc>>,
}

impl From<&TriggerDefinition> for TriggerSummary {
    fn from(t: &TriggerDefinition) -> Self {
        Self {
            id: t.id.clone(),
            name: t.name.clone(),
            enabled: t.enabled,
            token: t.token.clone(),
            last_event_at: t.last_event_at,
        }
    }
}

// ---------------------------------------------------------------------------
// TriggerRun
// ---------------------------------------------------------------------------

/// A record of a single trigger execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerRun {
    /// ID of the parent trigger.
    pub trigger_id: String,
    /// When the run started (also used as the unique run identifier).
    pub timestamp: DateTime<Utc>,
    /// Final status of this run.
    pub status: TriggerStatus,
    /// First 200 chars of the incoming payload (for display in history).
    pub payload_preview: String,
    /// Full LLM response text for this run.
    pub output: String,
    /// Wall-clock duration of the run in milliseconds.
    pub duration_ms: u64,
    /// Set when `status == Error`.
    pub error_message: Option<String>,
    /// IP address of the webhook caller (if available).
    pub source_ip: Option<String>,
}

// ---------------------------------------------------------------------------
// TriggerEvent
// ---------------------------------------------------------------------------

/// An incoming webhook event, passed from the bridge to the execution engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEvent {
    /// ID of the trigger this event belongs to.
    pub trigger_id: String,
    /// Workspace that owns the trigger.
    pub workspace_id: String,
    /// Raw payload body as a UTF-8 string.
    pub payload: String,
    /// Content-Type of the HTTP request.
    pub content_type: String,
    /// Selected HTTP request headers (sanitized).
    pub headers: HashMap<String, String>,
    /// When the bridge received the event.
    pub received_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// TriggerStore
// ---------------------------------------------------------------------------

/// Storage backend for webhook triggers and their run history.
///
/// All per-workspace operations are scoped to a `workspace_id`. The
/// `resolve_token` method must cross all workspaces to look up a token.
#[async_trait]
pub trait TriggerStore: Send + Sync {
    /// Create a new trigger in a workspace. The store assigns the UUID and
    /// timestamps; callers set `name`, `prompt_template`, `token`, etc.
    async fn create_trigger(
        &self,
        workspace_id: &str,
        trigger: &TriggerDefinition,
    ) -> Result<TriggerDefinition>;

    /// Load a trigger by its UUID.
    async fn get_trigger(
        &self,
        workspace_id: &str,
        trigger_id: &str,
    ) -> Result<TriggerDefinition>;

    /// List all triggers for a workspace (order unspecified).
    async fn list_triggers(&self, workspace_id: &str) -> Result<Vec<TriggerDefinition>>;

    /// Persist a modified trigger definition.
    async fn update_trigger(
        &self,
        workspace_id: &str,
        trigger: &TriggerDefinition,
    ) -> Result<TriggerDefinition>;

    /// Delete a trigger and all of its run history.
    async fn delete_trigger(&self, workspace_id: &str, trigger_id: &str) -> Result<()>;

    /// Resolve a webhook token across all workspaces.
    ///
    /// Returns `Some((workspace_id, trigger))` if the token is found and the
    /// trigger is enabled; `None` if the token is unknown or disabled.
    async fn resolve_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, TriggerDefinition)>>;

    /// Append a run record for a trigger.
    async fn write_run(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        run: &TriggerRun,
    ) -> Result<()>;

    /// Return up to `limit` most-recent runs (most recent first).
    /// Pass `usize::MAX` to return all runs.
    async fn list_runs(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        limit: usize,
    ) -> Result<Vec<TriggerRun>>;

    /// Remove the oldest runs that exceed `max_history`. Returns count deleted.
    /// A `max_history` of 0 means keep all runs (no pruning).
    async fn prune_runs(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        max_history: usize,
    ) -> Result<u64>;

    /// Generate a new token for an existing trigger (invalidates the old one).
    async fn regenerate_token(
        &self,
        workspace_id: &str,
        trigger_id: &str,
    ) -> Result<TriggerDefinition>;
}

// ---------------------------------------------------------------------------
// Token generation
// ---------------------------------------------------------------------------

/// Generate a cryptographic webhook token: 43 characters of base62, prefixed
/// with `wh_`. Provides ~256 bits of entropy.
pub fn generate_token() -> String {
    use rand::Rng;
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    let token: String = (0..43)
        .map(|_| BASE62[rng.gen_range(0..62usize)] as char)
        .collect();
    format!("wh_{}", token)
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

/// Render a prompt template by substituting `{{variable}}` placeholders.
///
/// Supported variables:
/// - `{{payload}}` — the event body
/// - `{{content_type}}` — the Content-Type header value
/// - `{{timestamp}}` — ISO 8601 receipt timestamp (UTC)
/// - `{{trigger_name}}` — the trigger's human-readable name
/// - `{{headers}}` — all HTTP request headers as `key: value` lines
pub fn render_template(template: &str, event: &TriggerEvent, trigger_name: &str) -> String {
    let headers_str = event
        .headers
        .iter()
        .map(|(k, v)| format!("{}: {}", k, v))
        .collect::<Vec<_>>()
        .join("\n");

    template
        .replace("{{payload}}", &event.payload)
        .replace("{{content_type}}", &event.content_type)
        .replace("{{timestamp}}", &event.received_at.to_rfc3339())
        .replace("{{trigger_name}}", trigger_name)
        .replace("{{headers}}", &headers_str)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_has_correct_prefix_and_length() {
        let token = generate_token();
        assert!(token.starts_with("wh_"), "token must start with 'wh_'");
        assert_eq!(token.len(), 3 + 43, "token must be 46 chars total");
    }

    #[test]
    fn token_is_alphanumeric() {
        let token = generate_token();
        assert!(token[3..].chars().all(|c| c.is_alphanumeric()));
    }

    #[test]
    fn tokens_are_unique() {
        let t1 = generate_token();
        let t2 = generate_token();
        assert_ne!(t1, t2);
    }

    #[test]
    fn template_substitution() {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());

        let event = TriggerEvent {
            trigger_id: "t1".to_string(),
            workspace_id: "w1".to_string(),
            payload: r#"{"key":"value"}"#.to_string(),
            content_type: "application/json".to_string(),
            headers,
            received_at: DateTime::parse_from_rfc3339("2026-02-15T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        };

        let template =
            "Payload: {{payload}}\nType: {{content_type}}\nAt: {{timestamp}}\nTrigger: {{trigger_name}}";
        let rendered = render_template(template, &event, "My Webhook");

        assert!(rendered.contains(r#"{"key":"value"}"#));
        assert!(rendered.contains("application/json"));
        assert!(rendered.contains("2026-02-15T10:00:00+00:00"));
        assert!(rendered.contains("My Webhook"));
    }

    #[test]
    fn trigger_summary_conversion() {
        let now = Utc::now();
        let trigger = TriggerDefinition {
            id: "t1".to_string(),
            name: "Email Processor".to_string(),
            prompt_template: "Summarize: {{payload}}".to_string(),
            token: generate_token(),
            enabled: true,
            notify: true,
            max_history: 20,
            rate_limit_per_minute: 10,
            hmac_secret: None,
            accepted_content_types: vec!["application/json".to_string()],
            created_at: now,
            updated_at: now,
            last_event_at: None,
        };

        let summary = TriggerSummary::from(&trigger);
        assert_eq!(summary.id, "t1");
        assert_eq!(summary.name, "Email Processor");
        assert!(summary.enabled);
        assert_eq!(summary.last_event_at, None);
    }

    #[test]
    fn trigger_status_display() {
        assert_eq!(TriggerStatus::Active.to_string(), "active");
        assert_eq!(TriggerStatus::Disabled.to_string(), "disabled");
        assert_eq!(TriggerStatus::Success.to_string(), "success");
        assert_eq!(TriggerStatus::Error.to_string(), "error");
    }

    #[test]
    fn trigger_definition_serde_roundtrip() {
        let now = Utc::now();
        let trigger = TriggerDefinition {
            id: "abc-123".to_string(),
            name: "Test".to_string(),
            prompt_template: "{{payload}}".to_string(),
            token: generate_token(),
            enabled: true,
            notify: false,
            max_history: 10,
            rate_limit_per_minute: 5,
            hmac_secret: Some("secret123".to_string()),
            accepted_content_types: vec!["text/plain".to_string()],
            created_at: now,
            updated_at: now,
            last_event_at: None,
        };

        let json = serde_json::to_string(&trigger).unwrap();
        let parsed: TriggerDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, trigger.id);
        assert_eq!(parsed.name, trigger.name);
        assert_eq!(parsed.token, trigger.token);
        assert_eq!(parsed.hmac_secret, trigger.hmac_secret);
    }
}
