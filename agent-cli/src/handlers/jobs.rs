use std::time::Instant;

use anyhow::{Context, Result};
use uuid::Uuid;

use agent_core::agent_loop::AgentLoopConfig;
use agent_core::scheduler::{JobDefinition, JobRun, JobStatus, JobSummary};
use agent_core::transport::JsonRpcId;
use agent_core::types::{Message, MessageContent, Role};
use agent_scheduler::validate_cron;

use crate::state::AgentState;

pub async fn handle_jobs_create(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let name = crate::require_param_str!(params, "name", id, state);
    let prompt = crate::require_param_str!(params, "prompt", id, state);
    let schedule = crate::require_param_str!(params, "schedule", id, state);

    if let Err(e) = validate_cron(&schedule) {
        return state
            .sender
            .send_error(
                id,
                agent_core::transport::error_codes::INVALID_PARAMS,
                format!("invalid cron expression: {}", e),
            )
            .await;
    }

    let p = params.as_ref();
    let timezone = p
        .and_then(|p| p.get("timezone"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let one_shot = p
        .and_then(|p| p.get("one_shot"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let notify = p
        .and_then(|p| p.get("notify"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let max_history = p
        .and_then(|p| p.get("max_history"))
        .and_then(|v| v.as_u64())
        .unwrap_or(7) as u32;

    let now = chrono::Utc::now();
    let job = JobDefinition {
        id: Uuid::new_v4().to_string(),
        name,
        prompt,
        schedule,
        timezone,
        enabled: true,
        one_shot,
        notify,
        max_history,
        created_at: now,
        updated_at: now,
        last_run_at: None,
    };

    let created = store.create_job(&workspace_id, &job).await
        .context("failed to create job")?;

    let result = serde_json::to_value(&created)
        .context("failed to serialize job")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_jobs_list(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);

    let jobs = store.list_jobs(&workspace_id).await
        .context("failed to list jobs")?;

    let summaries: Vec<JobSummary> = jobs.iter().map(JobSummary::from).collect();
    let result = serde_json::to_value(&summaries)
        .context("failed to serialize job list")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_jobs_get(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let job_id = crate::require_param_str!(params, "job_id", id, state);

    let job = store.get_job(&workspace_id, &job_id).await
        .context("job not found")?;

    let result = serde_json::to_value(&job)
        .context("failed to serialize job")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_jobs_update(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let job_id = crate::require_param_str!(params, "job_id", id, state);

    let mut job = store.get_job(&workspace_id, &job_id).await
        .context("job not found")?;

    let p = params.as_ref();
    if let Some(name) = p.and_then(|p| p.get("name")).and_then(|v| v.as_str()) {
        job.name = name.to_string();
    }
    if let Some(prompt) = p.and_then(|p| p.get("prompt")).and_then(|v| v.as_str()) {
        job.prompt = prompt.to_string();
    }
    if let Some(schedule) = p.and_then(|p| p.get("schedule")).and_then(|v| v.as_str()) {
        if let Err(e) = validate_cron(schedule) {
            return state
                .sender
                .send_error(
                    id,
                    agent_core::transport::error_codes::INVALID_PARAMS,
                    format!("invalid cron expression: {}", e),
                )
                .await;
        }
        job.schedule = schedule.to_string();
    }
    if let Some(tz) = p.and_then(|p| p.get("timezone")) {
        job.timezone = if tz.is_null() {
            None
        } else {
            tz.as_str().map(|s| s.to_string())
        };
    }
    if let Some(enabled) = p.and_then(|p| p.get("enabled")).and_then(|v| v.as_bool()) {
        job.enabled = enabled;
    }
    if let Some(one_shot) = p.and_then(|p| p.get("one_shot")).and_then(|v| v.as_bool()) {
        job.one_shot = one_shot;
    }
    if let Some(notify) = p.and_then(|p| p.get("notify")).and_then(|v| v.as_bool()) {
        job.notify = notify;
    }
    if let Some(max_history) = p.and_then(|p| p.get("max_history")).and_then(|v| v.as_u64()) {
        job.max_history = max_history as u32;
    }
    job.updated_at = chrono::Utc::now();

    let updated = store.update_job(&workspace_id, &job).await
        .context("failed to update job")?;

    let result = serde_json::to_value(&updated)
        .context("failed to serialize job")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_jobs_delete(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let job_id = crate::require_param_str!(params, "job_id", id, state);

    store.delete_job(&workspace_id, &job_id).await
        .context("failed to delete job")?;

    state.sender.send_response(id, serde_json::json!({})).await
}

pub async fn handle_jobs_history(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let job_id = crate::require_param_str!(params, "job_id", id, state);

    let limit = params
        .as_ref()
        .and_then(|p| p.get("limit"))
        .and_then(|v| v.as_u64())
        .unwrap_or(10) as usize;

    let runs = store.list_runs(&workspace_id, &job_id, limit).await
        .context("failed to list job runs")?;

    let result = serde_json::to_value(&runs)
        .context("failed to serialize runs")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_jobs_trigger(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = crate::require_scheduler_store!(state, id);
    let workspace_id = crate::require_param_str!(params, "workspace_id", id, state);
    let job_id = crate::require_param_str!(params, "job_id", id, state);

    let job = store.get_job(&workspace_id, &job_id).await
        .context("job not found")?;

    let runtime_guard = state.runtime.read().await;
    let provider = runtime_guard.active_provider()
        .context("no active LLM provider")?;

    let start = Instant::now();
    let timestamp = chrono::Utc::now();
    let run_id = Uuid::new_v4().to_string();

    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(job.prompt.clone()),
    }];
    let cancel = tokio_util::sync::CancellationToken::new();
    let loop_cfg = AgentLoopConfig { max_iterations: 25, ..AgentLoopConfig::default() };

    let run = match agent_core::agent_loop::run_agent_loop(
        provider,
        &messages,
        &[],
        None,
        &loop_cfg,
        cancel,
        None,
        &run_id,
        Some(&*runtime_guard),
    )
    .await
    {
        Ok(result) => {
            let output = result
                .new_messages
                .iter()
                .filter_map(|m| match &m.content {
                    MessageContent::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            JobRun {
                job_id: job.id.clone(),
                timestamp,
                status: JobStatus::Success,
                output,
                duration_ms: start.elapsed().as_millis() as u64,
                error_message: None,
            }
        }
        Err(e) => JobRun {
            job_id: job.id.clone(),
            timestamp,
            status: JobStatus::Error,
            output: String::new(),
            duration_ms: start.elapsed().as_millis() as u64,
            error_message: Some(e.to_string()),
        },
    };

    store.write_run(&workspace_id, &job.id, &run).await.ok();

    let mut updated_job = job.clone();
    updated_job.last_run_at = Some(run.timestamp);
    updated_job.updated_at = chrono::Utc::now();
    if job.one_shot && run.status == JobStatus::Success {
        updated_job.enabled = false;
    }
    store.update_job(&workspace_id, &updated_job).await.ok();

    if job.max_history > 0 {
        store.prune_runs(&workspace_id, &job.id, job.max_history as usize).await.ok();
    }

    let result = serde_json::to_value(&run)
        .context("failed to serialize run result")?;
    state.sender.send_response(id, result).await
}
