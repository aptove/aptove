//! Aptove Agent CLI
//!
//! Binary entry point. Provides two modes:
//! - `run` (default): ACP stdio mode for use with bridge or ACP clients
//! - `chat`: Interactive REPL with slash commands

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing::{error, info};

use agent_core::config::{self, AgentConfig};
use agent_core::plugin::PluginHost;
use agent_core::session::SessionManager;
use agent_core::system_prompt::{SystemPromptManager, DEFAULT_SYSTEM_PROMPT};
use agent_core::transport::{IncomingMessage, StdioTransport, TransportSender};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "aptove", version = "0.1.0", about = "Aptove — ACP AI Coding Agent")]
struct Cli {
    /// Path to config file (default: ~/.config/Aptove/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start in ACP stdio mode (default)
    Run,
    /// Interactive REPL chat mode
    Chat,
    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show the resolved configuration
    Show,
    /// Generate a sample config file
    Init,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // All logging goes to stderr (stdout is for JSON-RPC)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Load config
    let agent_config = if let Some(ref path) = cli.config {
        AgentConfig::load_from(path)?
    } else {
        AgentConfig::load_default()?
    };

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => run_acp_mode(agent_config).await,
        Commands::Chat => run_chat_mode(agent_config).await,
        Commands::Config { action } => run_config_command(action, agent_config),
    }
}

// ---------------------------------------------------------------------------
// ACP stdio mode
// ---------------------------------------------------------------------------

async fn run_acp_mode(config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in ACP mode");

    let plugin_host = setup_plugins(&config)?;
    let session_manager = SessionManager::new();
    let mut transport = StdioTransport::new();
    let sender = transport.sender();

    let state = Arc::new(AgentState {
        config: config.clone(),
        plugin_host: RwLock::new(plugin_host),
        session_manager,
        sender,
    });

    // Message dispatch loop
    while let Some(msg) = transport.recv().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(msg, &state).await {
                error!(err = %e, "error handling message");
            }
        });
    }

    info!("ACP transport closed, shutting down");
    state.plugin_host.read().await.shutdown_all().await?;
    Ok(())
}

/// Shared state for message handling.
struct AgentState {
    config: AgentConfig,
    plugin_host: RwLock<PluginHost>,
    session_manager: SessionManager,
    sender: TransportSender,
}

async fn handle_message(msg: IncomingMessage, state: &AgentState) -> Result<()> {
    match msg {
        IncomingMessage::Request { id, method, params } => {
            match method.as_str() {
                "initialize" => {
                    let model = state.config.model_for_provider(&state.config.provider);
                    let has_tools = !state.config.mcp_servers.is_empty();

                    let result = serde_json::json!({
                        "agent_info": {
                            "name": "Aptove",
                            "version": "0.1.0"
                        },
                        "protocol_version": "2025-07-09",
                        "capabilities": {
                            "sessions": {},
                            "tools": if has_tools { serde_json::json!({}) } else { serde_json::Value::Null },
                        },
                        "instructions": format!(
                            "Aptove agent using {} provider with model {}",
                            state.config.provider, model
                        )
                    });
                    state.sender.send_response(id, result).await?;
                }
                "session/new" => {
                    let provider_name = &state.config.provider;
                    let model = state.config.model_for_provider(provider_name);

                    // Get max tokens from the active provider
                    let max_tokens = {
                        let host = state.plugin_host.read().await;
                        match host.active_provider() {
                            Ok(p) => p.read().await.model_info().max_context_tokens,
                            Err(_) => 200_000, // fallback
                        }
                    };

                    let session_id = state
                        .session_manager
                        .create_session(max_tokens, provider_name, &model)
                        .await;

                    // Inject system prompt
                    let session_arc = state.session_manager.get_session(&session_id).await?;
                    {
                        let mut session = session_arc.lock().await;
                        let sys_prompt_mgr = SystemPromptManager::new(
                            state.config.system_prompt.default.as_deref()
                                .unwrap_or(DEFAULT_SYSTEM_PROMPT),
                            &std::env::current_dir().unwrap_or_default(),
                        );
                        let tools: Vec<String> = state.plugin_host.read().await
                            .tools()
                            .iter()
                            .map(|t| t.name.clone())
                            .collect();
                        let vars = SystemPromptManager::default_variables(
                            provider_name, &model, &tools,
                        );
                        let sys_msg = sys_prompt_mgr.build_message(None, &vars);
                        session.context.add_message(sys_msg);
                    }

                    let result = serde_json::json!({ "id": session_id });
                    state.sender.send_response(id, result).await?;
                }
                "session/prompt" => {
                    let session_id = params
                        .as_ref()
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let prompt_text = params
                        .as_ref()
                        .and_then(|p| p.get("prompt"))
                        .and_then(|p| p.get("content"))
                        .and_then(|c| {
                            // Handle both string and array content
                            if let Some(s) = c.as_str() {
                                Some(s.to_string())
                            } else if let Some(arr) = c.as_array() {
                                arr.iter()
                                    .filter_map(|item| {
                                        item.get("text").and_then(|t| t.as_str())
                                    })
                                    .next()
                                    .map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();

                    let sender = state.sender.clone();
                    let session_arc = state.session_manager.get_session(&session_id).await?;

                    // Add user message
                    {
                        let mut session = session_arc.lock().await;
                        session.add_user_message(&prompt_text);
                        session.reset_cancel();
                        session.is_busy = true;
                    }

                    // Run the agent loop
                    let provider = state.plugin_host.read().await.active_provider()?;
                    let tools = state.plugin_host.read().await.tools().to_vec();
                    let cancel_token = {
                        let session = session_arc.lock().await;
                        session.cancel_token.clone()
                    };
                    let messages = {
                        let session = session_arc.lock().await;
                        session.context.messages().to_vec()
                    };

                    let loop_config = agent_core::agent_loop::AgentLoopConfig {
                        max_iterations: state.config.agent.max_tool_iterations,
                    };

                    let result = agent_core::agent_loop::run_agent_loop(
                        provider,
                        &messages,
                        &tools,
                        None, // TODO: wire up MCP tool executor
                        &loop_config,
                        cancel_token,
                        Some(sender.clone()),
                        &session_id,
                    )
                    .await;

                    // Update session with new messages
                    match result {
                        Ok(loop_result) => {
                            {
                                let mut session = session_arc.lock().await;
                                for msg in &loop_result.new_messages {
                                    session.context.add_message(msg.clone());
                                }
                                session.is_busy = false;
                            }

                            let stop_reason = match loop_result.stop_reason {
                                agent_core::StopReason::EndTurn => "end_turn",
                                agent_core::StopReason::ToolUse => "tool_use",
                                agent_core::StopReason::MaxTokens => "max_tokens",
                                agent_core::StopReason::StopSequence => "stop_sequence",
                                agent_core::StopReason::Error => "error",
                            };

                            let response = serde_json::json!({
                                "id": session_id,
                                "stop_reason": stop_reason,
                                "usage": {
                                    "input_tokens": loop_result.total_usage.input_tokens,
                                    "output_tokens": loop_result.total_usage.output_tokens,
                                    "total_tokens": loop_result.total_usage.total_tokens,
                                }
                            });
                            sender.send_response(id, response).await?;
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
                                .await?;
                        }
                    }
                }
                _ => {
                    state
                        .sender
                        .send_error(
                            id,
                            agent_core::transport::error_codes::METHOD_NOT_FOUND,
                            format!("method not found: {}", method),
                        )
                        .await?;
                }
            }
        }
        IncomingMessage::Notification { method, params } => {
            match method.as_str() {
                "session/cancel" => {
                    if let Some(session_id) = params
                        .as_ref()
                        .and_then(|p| p.get("id"))
                        .and_then(|v| v.as_str())
                    {
                        let _ = state.session_manager.cancel_session(session_id).await;
                    }
                }
                _ => {
                    tracing::debug!(method = %method, "unhandled notification");
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Chat REPL mode
// ---------------------------------------------------------------------------

async fn run_chat_mode(config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in chat mode");

    // Validate config
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

    let model = config.model_for_provider(&config.provider);
    eprintln!("🤖 Aptove v0.1.0");
    eprintln!("   Provider: {} | Model: {}", config.provider, model);
    eprintln!("   Type /help for commands, /quit to exit\n");

    let stdin = tokio::io::stdin();
    let reader = tokio::io::BufReader::new(stdin);
    let mut lines = tokio::io::AsyncBufReadExt::lines(reader);

    loop {
        eprint!("> ");
        let line = match lines.next_line().await? {
            Some(l) => l.trim().to_string(),
            None => break,
        };

        if line.is_empty() {
            continue;
        }

        // Handle slash commands
        if line.starts_with('/') {
            match line.as_str() {
                "/quit" | "/exit" | "/q" => {
                    eprintln!("Goodbye!");
                    break;
                }
                "/help" | "/h" => {
                    eprintln!("Available commands:");
                    eprintln!("  /clear     - Clear context window");
                    eprintln!("  /context   - Show context window status");
                    eprintln!("  /model     - Show/switch model");
                    eprintln!("  /provider  - Show/switch provider");
                    eprintln!("  /usage     - Show session token usage");
                    eprintln!("  /sessions  - List saved sessions");
                    eprintln!("  /help      - Show this help");
                    eprintln!("  /quit      - Exit");
                }
                "/clear" => {
                    eprintln!("🧹 Context cleared.");
                }
                "/context" => {
                    eprintln!("📊 Context: (no active session in standalone chat mode yet)");
                }
                _ => {
                    eprintln!("Unknown command: {}. Type /help for available commands.", line);
                }
            }
            continue;
        }

        // Regular prompt — TODO: wire up to agent loop
        eprintln!("💭 (Chat mode prompt handling coming soon — use ACP mode with bridge for full functionality)");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Config command
// ---------------------------------------------------------------------------

fn run_config_command(action: ConfigAction, config: AgentConfig) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let toml_str = toml::to_string_pretty(&config)?;
            println!("{}", toml_str);
        }
        ConfigAction::Init => {
            let path = AgentConfig::default_path()?;
            if path.exists() {
                eprintln!("Config already exists at: {}", path.display());
                eprintln!("Edit it directly or delete it first.");
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, config::sample_config())?;
                eprintln!("✅ Config written to: {}", path.display());
                eprintln!("   Edit it to add your API key and configure MCP servers.");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin setup
// ---------------------------------------------------------------------------

fn setup_plugins(_config: &AgentConfig) -> Result<PluginHost> {
    let host = PluginHost::new();

    // TODO: Register providers based on config
    // if config.resolve_api_key("claude").is_some() {
    //     host.register_provider(Box::new(claude::ClaudeProvider::new(...)));
    // }

    Ok(host)
}
