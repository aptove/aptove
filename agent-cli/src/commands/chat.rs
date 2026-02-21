use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use agent_core::agent_loop::AgentLoopConfig;
use agent_core::config::AgentConfig;
use agent_core::persistence::SessionData;
use agent_core::system_prompt::{SystemPromptManager, DEFAULT_SYSTEM_PROMPT};
use agent_core::types::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult};

use crate::runtime::{build_runtime, setup_mcp_bridge};

pub async fn run_chat_mode(config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in chat mode");

    match config.validate() {
        Ok(warnings) => {
            for w in warnings {
                eprintln!("⚠ {}", w);
            }
        }
        Err(e) => {
            eprintln!("❌ Configuration error: {}", e);
            eprintln!("   Run `aptove config init` to set up your configuration.");
            std::process::exit(1);
        }
    }

    let mut runtime = build_runtime(config.clone()).await?;
    let mut provider_name = runtime.active_provider_name().to_string();
    let mut model = config.model_for_provider(&provider_name);

    let mut workspace = runtime.workspace_manager().default().await?;

    let mcp_bridge = setup_mcp_bridge(&config).await?;
    let tools: Vec<agent_core::types::ToolDefinition> = match &mcp_bridge {
        Some(b) => b.lock().await.all_tools(),
        None => Vec::new(),
    };

    let tool_executor: Option<agent_core::agent_loop::ToolExecutor> = mcp_bridge.as_ref().map(|bridge| {
        let bridge = bridge.clone();
        Arc::new(move |req: ToolCallRequest| {
            let bridge = bridge.clone();
            Box::pin(async move {
                let b = bridge.lock().await;
                b.execute_tool(req).await
            }) as futures::future::BoxFuture<'static, ToolCallResult>
        }) as agent_core::agent_loop::ToolExecutor
    });

    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace.uuid)
        .await
        .unwrap_or_else(|_| SessionData::new(&provider_name, &model));

    let sys_prompt_mgr = SystemPromptManager::new(
        config
            .system_prompt
            .default
            .as_deref()
            .unwrap_or(DEFAULT_SYSTEM_PROMPT),
        &std::env::current_dir().unwrap_or_default(),
    );
    let tool_names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
    let vars = SystemPromptManager::default_variables(&provider_name, &model, &tool_names);
    let sys_msg = sys_prompt_mgr.build_message(None, &vars);

    let mut messages: Vec<Message> = vec![sys_msg];
    for msg in &session_data.messages {
        messages.push(msg.clone());
    }

    let restored_count = session_data.messages.len();

    eprintln!("🤖 Aptove v0.1.0");
    eprintln!(
        "   Provider: {} | Model: {}",
        provider_name, model
    );
    eprintln!("   Workspace: {} ({})", workspace.name.as_deref().unwrap_or("default"), workspace.uuid);
    if !tools.is_empty() {
        eprintln!("   Tools: {} available", tools.len());
    }
    if restored_count > 0 {
        eprintln!("   Session: {} messages restored", restored_count);
    }
    eprintln!("   Type /help for commands, /quit to exit\n");

    let mut provider = runtime.active_provider()
        .context("no active LLM provider")?;
    let loop_config = AgentLoopConfig {
        max_iterations: config.agent.max_tool_iterations,
        ..AgentLoopConfig::default()
    };

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = tokio::io::AsyncBufReadExt::lines(reader);

    loop {
        eprint!("{}> ", provider_name);
        let line = match lines.next_line().await? {
            Some(l) => l.trim().to_string(),
            None => break,
        };

        if line.is_empty() {
            continue;
        }

        if line.starts_with('/') {
            match line.as_str() {
                "/quit" | "/exit" | "/q" => {
                    eprintln!("Goodbye!");
                    break;
                }
                "/help" | "/h" => {
                    eprintln!("Available commands:");
                    eprintln!("  /clear           - Clear conversation and session.md");
                    eprintln!("  /context         - Show context window status");
                    eprintln!("  /workspace       - Show current workspace");
                    eprintln!("  /sessions        - List all saved sessions");
                    eprintln!("  /load <id>       - Resume a saved session by workspace ID");
                    eprintln!("  /model <name>    - Switch model within the current provider");
                    eprintln!("  /provider <name> - Switch to a different provider");
                    eprintln!("  /mcp             - List connected MCP servers and their tools");
                    eprintln!("  /help            - Show this help");
                    eprintln!("  /quit            - Exit");
                }
                "/workspace" => {
                    eprintln!(
                        "📁 Workspace: {} ({})",
                        workspace.name.as_deref().unwrap_or("unnamed"),
                        workspace.uuid
                    );
                    eprintln!("   Provider: {} | Model: {}", provider_name, model);
                    eprintln!("   Messages: {}", messages.len() - 1);
                }
                "/clear" => {
                    if let Err(e) = runtime
                        .workspace_manager()
                        .session_store()
                        .clear(&workspace.uuid)
                        .await
                    {
                        eprintln!("⚠ Failed to clear session file: {}", e);
                    }
                    messages.truncate(1);
                    eprintln!("🧹 Conversation cleared (session.md reset).");
                }
                "/context" => {
                    let msg_count = messages.len() - 1;
                    eprintln!("📊 Context: {} messages ({} user + assistant turns)", msg_count, msg_count / 2);
                }
                "/sessions" => {
                    let workspaces = runtime
                        .workspace_manager()
                        .list()
                        .await
                        .unwrap_or_default();
                    if workspaces.is_empty() {
                        eprintln!("No saved sessions found.");
                    } else {
                        eprintln!(
                            "{:<38} {:<18} {:>8}  {}",
                            "ID", "DATE", "MESSAGES", "PROVIDER"
                        );
                        eprintln!("{}", "-".repeat(76));
                        for ws in &workspaces {
                            let session_data = runtime
                                .workspace_manager()
                                .session_store()
                                .read(&ws.uuid)
                                .await
                                .unwrap_or_else(|_| {
                                    SessionData::new("-", "-")
                                });
                            let msg_count = session_data.messages.len();
                            let provider = if ws.provider.as_deref().unwrap_or("") != "" {
                                ws.provider.as_deref().unwrap_or("-").to_string()
                            } else {
                                session_data.frontmatter.provider.clone()
                            };
                            eprintln!(
                                "{:<38} {:<18} {:>8}  {}",
                                ws.uuid,
                                ws.last_accessed.format("%Y-%m-%d %H:%M"),
                                msg_count,
                                provider,
                            );
                        }
                    }
                }
                _ if line.starts_with("/load") => {
                    let ws_id = line.strip_prefix("/load").map(|s| s.trim()).unwrap_or("").to_string();
                    if ws_id.is_empty() {
                        eprintln!("Usage: /load <workspace-id>");
                        eprintln!("       Use /sessions to list available session IDs.");
                    } else {
                        match runtime
                            .workspace_manager()
                            .workspace_store()
                            .load(&ws_id)
                            .await
                        {
                            Err(_) => {
                                eprintln!("❌ Session '{}' not found.", ws_id);
                                eprintln!("   Use /sessions to list available session IDs.");
                            }
                            Ok(loaded_ws) => {
                                let session_data = runtime
                                    .workspace_manager()
                                    .session_store()
                                    .read(&ws_id)
                                    .await
                                    .unwrap_or_else(|_| {
                                        SessionData::new(&provider_name, &model)
                                    });

                                let sys_prompt_mgr = SystemPromptManager::new(
                                    config
                                        .system_prompt
                                        .default
                                        .as_deref()
                                        .unwrap_or(DEFAULT_SYSTEM_PROMPT),
                                    &std::env::current_dir().unwrap_or_default(),
                                );
                                let tool_names: Vec<String> =
                                    tools.iter().map(|t| t.name.clone()).collect();
                                let vars = SystemPromptManager::default_variables(
                                    &provider_name,
                                    &model,
                                    &tool_names,
                                );
                                let sys_msg = sys_prompt_mgr.build_message(None, &vars);

                                messages = vec![sys_msg];
                                for msg in &session_data.messages {
                                    messages.push(msg.clone());
                                }

                                let restored = session_data.messages.len();
                                workspace = loaded_ws;

                                eprintln!(
                                    "✅ Loaded session {} ({} messages, provider: {})",
                                    ws_id,
                                    restored,
                                    session_data.frontmatter.provider,
                                );
                            }
                        }
                    }
                }
                _ if line.starts_with("/model") => {
                    let new_model = line.strip_prefix("/model").map(|s| s.trim()).unwrap_or("").to_string();
                    if new_model.is_empty() {
                        eprintln!("Current model: {}", model);
                        eprintln!("Usage: /model <model-name>");
                    } else {
                        model = new_model;
                        let new_runtime = build_runtime({
                            let mut c = config.clone();
                            match provider_name.as_str() {
                                "claude" => {
                                    if let Some(ref mut p) = c.providers.claude { p.model = Some(model.clone()); }
                                }
                                "gemini" => {
                                    if let Some(ref mut p) = c.providers.gemini { p.model = Some(model.clone()); }
                                }
                                "openai" => {
                                    if let Some(ref mut p) = c.providers.openai { p.model = Some(model.clone()); }
                                }
                                _ => {}
                            }
                            c
                        }).await;
                        match new_runtime {
                            Ok(rt) => {
                                runtime = rt;
                                match runtime.active_provider() {
                                    Ok(p) => {
                                        provider = p;
                                        eprintln!("✅ Model switched to: {}", model);
                                    }
                                    Err(e) => eprintln!("❌ Failed to get provider: {}", e),
                                }
                            }
                            Err(e) => eprintln!("❌ Failed to switch model: {}", e),
                        }
                    }
                }
                _ if line.starts_with("/provider") => {
                    let new_provider = line.strip_prefix("/provider").map(|s| s.trim()).unwrap_or("").to_string();
                    if new_provider.is_empty() {
                        let mut names = runtime.provider_names();
                        names.sort();
                        eprintln!("Current provider: {}", provider_name);
                        eprintln!("Available providers: {}", names.join(", "));
                        eprintln!("Usage: /provider <name>");
                    } else {
                        match runtime.set_active_provider(&new_provider) {
                            Ok(()) => {
                                match runtime.active_provider() {
                                    Ok(p) => {
                                        provider = p;
                                        provider_name = new_provider.clone();
                                        model = config.model_for_provider(&provider_name);
                                        eprintln!(
                                            "✅ Switched to provider: {} (model: {})",
                                            provider_name, model
                                        );
                                    }
                                    Err(e) => eprintln!("❌ Failed to get provider: {}", e),
                                }
                            }
                            Err(e) => eprintln!("❌ {}", e),
                        }
                    }
                }
                "/mcp" => {
                    match &mcp_bridge {
                        None => eprintln!("No MCP servers configured."),
                        Some(b) => {
                            let bridge = b.lock().await;
                            let servers = bridge.server_info();
                            if servers.is_empty() {
                                eprintln!("No MCP servers connected.");
                            } else {
                                for (server, alive, tool_names) in &servers {
                                    let status = if *alive { "🟢 connected" } else { "🔴 disconnected" };
                                    eprintln!("{} {} ({} tools)", status, server, tool_names.len());
                                    for tool in tool_names {
                                        eprintln!("    • {}", tool);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {
                    eprintln!(
                        "Unknown command: {}. Type /help for available commands.",
                        line
                    );
                }
            }
            continue;
        }

        let user_msg = Message {
            role: Role::User,
            content: MessageContent::Text(line.clone()),
        };
        messages.push(user_msg.clone());

        let _ = runtime
            .workspace_manager()
            .session_store()
            .append(&workspace.uuid, &user_msg)
            .await;

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let result = agent_core::agent_loop::run_agent_loop(
            provider.clone(),
            &messages,
            &tools,
            tool_executor.clone(),
            &loop_config,
            cancel_token,
            None,
            "chat",
            Some(&runtime),
        )
        .await;

        match result {
            Ok(loop_result) => {
                for msg in &loop_result.new_messages {
                    match &msg.content {
                        MessageContent::Text(text) => {
                            if msg.role == Role::Assistant {
                                eprintln!("\n{}\n", text);
                            }
                        }
                        MessageContent::ToolCalls(calls) => {
                            for call in calls {
                                eprintln!("🔧 Tool: {} → {}", call.name, call.id);
                            }
                        }
                        MessageContent::ToolResult(result) => {
                            let preview = if result.content.len() > 200 {
                                format!("{}…", &result.content[..200])
                            } else {
                                result.content.clone()
                            };
                            eprintln!("   ↳ {}", preview);
                        }
                    }

                    messages.push(msg.clone());

                    let _ = runtime
                        .workspace_manager()
                        .session_store()
                        .append(&workspace.uuid, msg)
                        .await;
                }

                if loop_result.total_usage.input_tokens > 0 || loop_result.total_usage.output_tokens > 0 {
                    eprintln!(
                        "   [{} in / {} out tokens]",
                        loop_result.total_usage.input_tokens,
                        loop_result.total_usage.output_tokens
                    );
                }
            }
            Err(e) => {
                eprintln!("❌ Error: {:#}", e);
            }
        }
    }

    runtime.shutdown_plugins().await?;
    Ok(())
}
