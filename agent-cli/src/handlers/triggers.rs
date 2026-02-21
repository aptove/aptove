use anyhow::{Context, Result};
use uuid::Uuid;

use agent_core::transport::JsonRpcId;
use agent_core::trigger::{TriggerDefinition, TriggerSummary};

use crate::state::AgentState;

pub async fn handle_triggers_create(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let name = crate::require_param_str!(params, "name", id, state);
    let prompt_template = crate::require_param_str!(params, "prompt_template", id, state);

    let p = params.as_ref();
    let rate_limit_per_minute = p
        .and_then(|p| p.get("rate_limit_per_minute"))
        .and_then(|v| v.as_u64())
        .unwrap_or(60) as u32;
    let notify = p
        .and_then(|p| p.get("notify"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let max_history = p
        .and_then(|p| p.get("max_history"))
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as u32;
    let hmac_secret = p
        .and_then(|p| p.get("hmac_secret"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let accepted_content_types: Vec<String> = p
        .and_then(|p| p.get("accepted_content_types"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let now = chrono::Utc::now();
    let trigger = TriggerDefinition {
        id: Uuid::new_v4().to_string(),
        name,
        prompt_template,
        token: agent_core::trigger::generate_token(),
        enabled: true,
        notify,
        max_history,
        rate_limit_per_minute,
        hmac_secret,
        accepted_content_types,
        created_at: now,
        updated_at: now,
        last_event_at: None,
    };

    let created = store.create_trigger(&workspace_id, &trigger).await
        .context("failed to create trigger")?;

    let result = serde_json::to_value(&created)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_triggers_list(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);

    let triggers = store.list_triggers(&workspace_id).await
        .context("failed to list triggers")?;

    let summaries: Vec<TriggerSummary> = triggers.iter().map(TriggerSummary::from).collect();
    let result = serde_json::to_value(&summaries)
        .context("failed to serialize trigger list")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_triggers_get(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let trigger_id = crate::require_param_str!(params, "trigger_id", id, state);

    let trigger = store.get_trigger(&workspace_id, &trigger_id).await
        .context("trigger not found")?;

    let result = serde_json::to_value(&trigger)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_triggers_update(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let trigger_id = crate::require_param_str!(params, "trigger_id", id, state);

    let mut trigger = store.get_trigger(&workspace_id, &trigger_id).await
        .context("trigger not found")?;

    let p = params.as_ref();
    if let Some(name) = p.and_then(|p| p.get("name")).and_then(|v| v.as_str()) {
        trigger.name = name.to_string();
    }
    if let Some(pt) = p.and_then(|p| p.get("prompt_template")).and_then(|v| v.as_str()) {
        trigger.prompt_template = pt.to_string();
    }
    if let Some(enabled) = p.and_then(|p| p.get("enabled")).and_then(|v| v.as_bool()) {
        trigger.enabled = enabled;
    }
    if let Some(notify) = p.and_then(|p| p.get("notify")).and_then(|v| v.as_bool()) {
        trigger.notify = notify;
    }
    if let Some(rl) = p.and_then(|p| p.get("rate_limit_per_minute")).and_then(|v| v.as_u64()) {
        trigger.rate_limit_per_minute = rl as u32;
    }
    if let Some(mh) = p.and_then(|p| p.get("max_history")).and_then(|v| v.as_u64()) {
        trigger.max_history = mh as u32;
    }
    if let Some(sec) = p.and_then(|p| p.get("hmac_secret")) {
        trigger.hmac_secret = if sec.is_null() {
            None
        } else {
            sec.as_str().map(|s| s.to_string())
        };
    }
    if let Some(cts) = p.and_then(|p| p.get("accepted_content_types")).and_then(|v| v.as_array()) {
        trigger.accepted_content_types = cts.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect();
    }
    trigger.updated_at = chrono::Utc::now();

    let updated = store.update_trigger(&workspace_id, &trigger).await
        .context("failed to update trigger")?;

    let result = serde_json::to_value(&updated)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_triggers_delete(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let trigger_id = crate::require_param_str!(params, "trigger_id", id, state);

    store.delete_trigger(&workspace_id, &trigger_id).await
        .context("failed to delete trigger")?;

    state.sender.send_response(id, serde_json::json!({})).await
}

pub async fn handle_triggers_history(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let trigger_id = crate::require_param_str!(params, "trigger_id", id, state);

    let limit = params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;

    let runs = store.list_runs(&workspace_id, &trigger_id, limit).await
        .context("failed to list trigger runs")?;

    let result = serde_json::to_value(&runs)
        .context("failed to serialize runs")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_triggers_regenerate_token(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_trigger_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let trigger_id = crate::require_param_str!(params, "trigger_id", id, state);

    let trigger = store.regenerate_token(&workspace_id, &trigger_id).await
        .context("failed to regenerate token")?;

    let result = serde_json::to_value(&trigger)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
}
