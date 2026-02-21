use anyhow::Result;
use tracing::{error, info, warn};

use agent_core::transport::IncomingMessage;
use agent_core::trigger::TriggerEvent;
use agent_scheduler::Scheduler;

use crate::handlers::jobs::{
    handle_jobs_create, handle_jobs_delete, handle_jobs_get, handle_jobs_history,
    handle_jobs_list, handle_jobs_trigger, handle_jobs_update,
};
use crate::handlers::session::{
    handle_initialize, handle_session_load, handle_session_new, handle_session_prompt,
};
use crate::handlers::triggers::{
    handle_triggers_create, handle_triggers_delete, handle_triggers_get, handle_triggers_history,
    handle_triggers_list, handle_triggers_regenerate_token, handle_triggers_update,
};
use crate::state::AgentState;

pub async fn handle_message(msg: IncomingMessage, state: &AgentState) -> Result<()> {
    match msg {
        IncomingMessage::Request { id, method, params } => {
            let result = match method.as_str() {
                "initialize" => handle_initialize(id.clone(), state).await,
                "session/new" => handle_session_new(id.clone(), &params, state).await,
                "session/load" => handle_session_load(id.clone(), &params, state).await,
                "session/prompt" => handle_session_prompt(id.clone(), &params, state).await,
                "jobs/create" => handle_jobs_create(id.clone(), &params, state).await,
                "jobs/list" => handle_jobs_list(id.clone(), &params, state).await,
                "jobs/get" => handle_jobs_get(id.clone(), &params, state).await,
                "jobs/update" => handle_jobs_update(id.clone(), &params, state).await,
                "jobs/delete" => handle_jobs_delete(id.clone(), &params, state).await,
                "jobs/history" => handle_jobs_history(id.clone(), &params, state).await,
                "jobs/trigger" => handle_jobs_trigger(id.clone(), &params, state).await,
                "triggers/create" => handle_triggers_create(id.clone(), &params, state).await,
                "triggers/list" => handle_triggers_list(id.clone(), &params, state).await,
                "triggers/get" => handle_triggers_get(id.clone(), &params, state).await,
                "triggers/update" => handle_triggers_update(id.clone(), &params, state).await,
                "triggers/delete" => handle_triggers_delete(id.clone(), &params, state).await,
                "triggers/history" => handle_triggers_history(id.clone(), &params, state).await,
                "triggers/regenerate_token" => handle_triggers_regenerate_token(id.clone(), &params, state).await,
                _ => {
                    return state
                        .sender
                        .send_error(
                            id,
                            agent_core::transport::error_codes::METHOD_NOT_FOUND,
                            format!("method not found: {}", method),
                        )
                        .await;
                }
            };

            if let Err(ref e) = result {
                let error_msg = format!("{:#}", e);
                error!(method = %method, err = %error_msg, "request handler failed");
                let _ = state
                    .sender
                    .send_error(
                        id,
                        agent_core::transport::error_codes::INTERNAL_ERROR,
                        error_msg,
                    )
                    .await;
            }
            result
        }
        IncomingMessage::Notification { method, params } => {
            if method == "session/cancel" {
                if let Some(session_id) = params
                    .as_ref()
                    .and_then(|p| p.get("id"))
                    .and_then(|v| v.as_str())
                {
                    let _ = state.session_manager.cancel_session(session_id).await;
                }
            } else if method == "triggers/execute" {
                // Fire-and-forget: bridge sends this when a webhook event arrives.
                if let Some(ref p) = params {
                    if let Ok(event) = serde_json::from_value::<TriggerEvent>(p.clone()) {
                        let trigger_store = state.trigger_store.clone();
                        let scheduler_store = state.scheduler_store.clone();
                        let runtime = state.runtime.read().await;
                        let provider = runtime.active_provider().ok();
                        let ws_store = runtime.workspace_manager().workspace_store().clone();
                        drop(runtime);
                        if let (Some(tstore), Some(sstore), Some(provider)) =
                            (trigger_store, scheduler_store, provider)
                        {
                            tokio::spawn(async move {
                                match tstore.get_trigger(&event.workspace_id, &event.trigger_id).await {
                                    Ok(trigger) => {
                                        let scheduler = Scheduler::new(sstore, ws_store, provider);
                                        let run = scheduler.execute_trigger(&trigger, &event).await;
                                        tstore.write_run(&event.workspace_id, &event.trigger_id, &run).await.ok();
                                        let mut updated = trigger.clone();
                                        updated.last_event_at = Some(event.received_at);
                                        updated.updated_at = chrono::Utc::now();
                                        tstore.update_trigger(&event.workspace_id, &updated).await.ok();
                                        if trigger.max_history > 0 {
                                            tstore.prune_runs(&event.workspace_id, &event.trigger_id, trigger.max_history as usize).await.ok();
                                        }
                                        info!(trigger_id = %event.trigger_id, status = %run.status, "trigger executed");
                                    }
                                    Err(e) => {
                                        warn!(trigger_id = %event.trigger_id, err = %e, "trigger not found for execution");
                                    }
                                }
                            });
                        }
                    }
                }
            } else {
                tracing::debug!(method = %method, "unhandled notification");
            }
            Ok(())
        }
    }
}
