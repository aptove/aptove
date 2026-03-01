use anyhow::{Context, Result};
use tracing::{info, warn};
use uuid::Uuid;

use agent_core::config;
use agent_core::agent_loop::{AgentLoopConfig, ToolExecutor};
use agent_core::persistence::SessionData;
use agent_core::system_prompt::{SystemPromptManager, DEFAULT_SYSTEM_PROMPT};
use agent_core::transport::JsonRpcId;
use agent_core::types::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult};
use agent_core::StopReason;
use agent_client_protocol_schema::{
    AgentCapabilities, ContentBlock, Implementation, InitializeResponse, NewSessionResponse,
    PromptRequest, PromptResponse, ProtocolVersion, StopReason as AcpStopReason,
};

use crate::state::AgentState;

pub async fn handle_initialize(id: JsonRpcId, state: &AgentState) -> Result<()> {
    let response = InitializeResponse::new(ProtocolVersion::LATEST)
        .agent_info(Implementation::new("Aptove", env!("CARGO_PKG_VERSION")))
        .agent_capabilities(AgentCapabilities::default());

    let result = serde_json::to_value(&response)
        .context("failed to serialize initialize response")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_session_new(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let runtime = state.runtime.read().await;

    // Extract optional sessionId from _meta for session persistence
    let session_id_from_meta = params
        .as_ref()
        .and_then(|p| p.get("_meta"))
        .and_then(|m| {
            info!("received _meta field: {:?}", m);
            m.get("sessionId")
        })
        .and_then(|v| {
            info!("extracted sessionId from _meta: {:?}", v);
            v.as_str()
        })
        .map(|s| {
            info!("parsed sessionId: {}", s);
            s.to_string()
        });

    // Extract optional device_id for workspace resolution (fallback if no sessionId)
    let device_id = params
        .as_ref()
        .and_then(|p| p.get("device_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let session_id_to_use = if let Some(sid) = session_id_from_meta {
        sid
    } else if device_id == "default" {
        let new_id = Uuid::new_v4().to_string();
        info!(session_id = %new_id, "generated new session ID for mobile app");
        new_id
    } else {
        String::new()
    };

    // Resolve workspace based on sessionId or device_id
    let (workspace, is_reusing_session) = if !session_id_to_use.is_empty() {
        info!(session_id = %session_id_to_use, "using sessionId as workspace UUID");
        match runtime.workspace_manager().workspace_store().load(&session_id_to_use).await {
            Ok(ws) => {
                info!(workspace = %ws.uuid, "loaded existing workspace for sessionId (reusing session)");
                (ws, true)
            }
            Err(_) => {
                info!(session_id = %session_id_to_use, "creating new workspace with sessionId as UUID");
                let ws = runtime.workspace_manager()
                    .workspace_store()
                    .create(&session_id_to_use, None, None)
                    .await
                    .with_context(|| format!("failed to create workspace with UUID '{}'", session_id_to_use))?;
                (ws, false)
            }
        }
    } else {
        (runtime.workspace_manager().resolve(device_id).await
            .with_context(|| format!("failed to resolve workspace for device '{}'", device_id))?, false)
    };

    info!(
        workspace = %workspace.uuid,
        is_reusing_session,
        "resolved workspace for session"
    );

    // Load workspace-specific config overlay (if any)
    let ws_config = match runtime.workspace_manager().load_workspace_config(&workspace.uuid).await {
        Ok(Some(ws_toml)) => {
            match config::merge_toml_configs(
                &toml::to_string(&state.config)?,
                &ws_toml,
            ) {
                Ok(merged) => {
                    info!(workspace = %workspace.uuid, provider = %merged.provider, "using workspace config overlay");
                    merged
                }
                Err(e) => {
                    tracing::warn!(workspace = %workspace.uuid, err = %e, "failed to merge workspace config, using global");
                    state.config.clone()
                }
            }
        }
        _ => state.config.clone(),
    };

    let provider_name = workspace.provider.as_deref()
        .unwrap_or(ws_config.provider.as_str())
        .to_string();
    let model = ws_config.model_for_provider(&provider_name);

    if is_reusing_session {
        info!(workspace = %workspace.uuid, "clearing session history for reused session");
        if let Err(e) = runtime
            .workspace_manager()
            .session_store()
            .clear(&workspace.uuid)
            .await
        {
            warn!(workspace = %workspace.uuid, err = %e, "failed to clear session history");
        }
    }

    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace.uuid)
        .await
        .unwrap_or_else(|_| SessionData::new(&provider_name, &model));

    let max_tokens = match runtime.active_provider() {
        Ok(p) => p.model_info().max_context_tokens,
        Err(_) => 200_000,
    };

    let session_id = if !session_id_to_use.is_empty() {
        state
            .session_manager
            .create_session_with_id(session_id_to_use, max_tokens, &provider_name, &model)
            .await
    } else {
        state
            .session_manager
            .create_session(max_tokens, &provider_name, &model)
            .await
    };

    state
        .session_workspaces
        .write()
        .await
        .insert(session_id.clone(), workspace.uuid.clone());

    let session_arc = state.session_manager.get_session(&session_id).await?;
    {
        let mut session = session_arc.lock().await;

        let sys_prompt_mgr = SystemPromptManager::new(
            ws_config
                .system_prompt
                .default
                .as_deref()
                .unwrap_or(DEFAULT_SYSTEM_PROMPT),
            &std::env::current_dir().unwrap_or_default(),
        );
        let tools: Vec<String> = runtime.tools().iter().map(|t| t.name.clone()).collect();
        let vars = SystemPromptManager::default_variables(&provider_name, &model, &tools);
        let sys_msg = sys_prompt_mgr.build_message(None, &vars);
        session.context.add_message(sys_msg);

        for msg in &session_data.messages {
            session.context.add_message(msg.clone());
        }
    }

    let result = serde_json::to_value(&NewSessionResponse::new(session_id))
        .context("failed to serialize session/new response")?;
    state.sender.send_response(id, result).await
}

pub async fn handle_session_prompt(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let prompt_req: PromptRequest = params
        .as_ref()
        .map(|p| serde_json::from_value::<PromptRequest>(p.clone()))
        .transpose()
        .context("failed to parse session/prompt request")?
        .context("session/prompt requires parameters")?;

    let session_id = prompt_req.session_id.0.to_string();

    let prompt_text = prompt_req
        .prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(tc) => Some(tc.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    info!(
        session_id = %session_id,
        prompt_len = prompt_text.len(),
        "received session/prompt"
    );

    let workspace_uuid = state
        .session_workspaces
        .read()
        .await
        .get(&session_id)
        .cloned();

    let sender = state.sender.clone();
    let session_arc = state.session_manager.get_session(&session_id).await
        .with_context(|| format!("session '{}' not found", session_id))?;

    {
        let mut session = session_arc.lock().await;
        session.add_user_message(&prompt_text);
        session.reset_cancel();
        session.is_busy = true;
    }

    if let Some(ref ws_uuid) = workspace_uuid {
        let runtime = state.runtime.read().await;
        let user_msg = Message {
            role: Role::User,
            content: MessageContent::Text(prompt_text.clone()),
        };
        let _ = runtime
            .workspace_manager()
            .session_store()
            .append(ws_uuid, &user_msg)
            .await;
    }

    let runtime = state.runtime.read().await;
    let provider = runtime.active_provider()
        .context("no active LLM provider available")?;
    let tools = runtime.tools().to_vec();
    let cancel_token = {
        let session = session_arc.lock().await;
        session.cancel_token.clone()
    };
    let messages = {
        let session = session_arc.lock().await;
        session.context.messages().to_vec()
    };
    let loop_config = AgentLoopConfig {
        max_iterations: state.config.agent.max_tool_iterations,
        ..AgentLoopConfig::default()
    };

    let tool_executor: Option<ToolExecutor> = state.mcp_bridge.as_ref().map(|bridge| {
        let bridge = bridge.clone();
        std::sync::Arc::new(move |req: ToolCallRequest| {
            let bridge = bridge.clone();
            Box::pin(async move {
                let b = bridge.lock().await;
                b.execute_tool(req).await
            }) as futures::future::BoxFuture<'static, ToolCallResult>
        }) as ToolExecutor
    });

    drop(runtime);

    let runtime_guard = state.runtime.read().await;
    let result = agent_core::agent_loop::run_agent_loop(
        provider,
        &messages,
        &tools,
        tool_executor,
        &loop_config,
        cancel_token,
        Some(sender.clone()),
        &session_id,
        Some(&*runtime_guard),
    )
    .await;
    drop(runtime_guard);

    match result {
        Ok(loop_result) => {
            {
                let mut session = session_arc.lock().await;
                for msg in &loop_result.new_messages {
                    session.context.add_message(msg.clone());
                }
                session.is_busy = false;
            }

            if let Some(ref ws_uuid) = workspace_uuid {
                let runtime = state.runtime.read().await;
                for msg in &loop_result.new_messages {
                    let _ = runtime
                        .workspace_manager()
                        .session_store()
                        .append(ws_uuid, msg)
                        .await;
                }
            }

            let acp_stop_reason = match loop_result.stop_reason {
                StopReason::EndTurn => AcpStopReason::EndTurn,
                StopReason::ToolUse => AcpStopReason::EndTurn,
                StopReason::MaxTokens => AcpStopReason::MaxTokens,
                StopReason::StopSequence => AcpStopReason::EndTurn,
                StopReason::Error => AcpStopReason::Cancelled,
            };

            let response = serde_json::to_value(&PromptResponse::new(acp_stop_reason))
                .context("failed to serialize session/prompt response")?;
            info!(session_id = %session_id, "sending session/prompt response");
            sender.send_response(id, response).await
        }
        Err(e) => {
            let mut session = session_arc.lock().await;
            session.is_busy = false;
            sender
                .send_error(
                    id,
                    agent_core::transport::error_codes::INTERNAL_ERROR,
                    e.to_string(),
                )
                .await
        }
    }
}

pub async fn handle_session_load(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let workspace_id = params
        .as_ref()
        .and_then(|p| p.get("workspace_id").or_else(|| p.get("session_id")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("session/load requires a 'workspace_id' parameter"))?
        .to_string();

    let runtime = state.runtime.read().await;

    let workspace = runtime
        .workspace_manager()
        .workspace_store()
        .load(&workspace_id)
        .await
        .with_context(|| format!("workspace '{}' not found", workspace_id))?;

    let _ = runtime
        .workspace_manager()
        .workspace_store()
        .update_accessed(&workspace_id)
        .await;

    info!(workspace = %workspace.uuid, "loading session from store");

    let ws_config = match runtime
        .workspace_manager()
        .load_workspace_config(&workspace.uuid)
        .await
    {
        Ok(Some(ws_toml)) => {
            match config::merge_toml_configs(&toml::to_string(&state.config)?, &ws_toml) {
                Ok(merged) => merged,
                Err(e) => {
                    tracing::warn!(workspace = %workspace.uuid, err = %e, "failed to merge workspace config, using global");
                    state.config.clone()
                }
            }
        }
        _ => state.config.clone(),
    };

    let provider_name = workspace
        .provider
        .as_deref()
        .unwrap_or(ws_config.provider.as_str())
        .to_string();
    let model = ws_config.model_for_provider(&provider_name);

    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace_id)
        .await
        .unwrap_or_else(|_| SessionData::new(&provider_name, &model));

    info!(
        workspace = %workspace_id,
        message_count = session_data.messages.len(),
        provider = %session_data.frontmatter.provider,
        "restored session data"
    );

    let max_tokens = match runtime.active_provider() {
        Ok(p) => p.model_info().max_context_tokens,
        Err(_) => 200_000,
    };

    let session_id = state
        .session_manager
        .create_session_with_id(workspace_id.clone(), max_tokens, &provider_name, &model)
        .await;

    state
        .session_workspaces
        .write()
        .await
        .insert(session_id.clone(), workspace_id.clone());

    let session_arc = state.session_manager.get_session(&session_id).await?;
    {
        let mut session = session_arc.lock().await;

        let sys_prompt_mgr = SystemPromptManager::new(
            ws_config
                .system_prompt
                .default
                .as_deref()
                .unwrap_or(DEFAULT_SYSTEM_PROMPT),
            &std::env::current_dir().unwrap_or_default(),
        );
        let tools: Vec<String> = runtime.tools().iter().map(|t| t.name.clone()).collect();
        let vars = SystemPromptManager::default_variables(&provider_name, &model, &tools);
        let sys_msg = sys_prompt_mgr.build_message(None, &vars);
        session.context.add_message(sys_msg);

        for msg in &session_data.messages {
            session.context.add_message(msg.clone());
        }
    }

    let result = serde_json::to_value(&NewSessionResponse::new(session_id))
        .context("failed to serialize session/load response")?;
    state.sender.send_response(id, result).await
}
