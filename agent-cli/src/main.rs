//! Aptove Agent CLI
//!
//! Binary entry point. Provides multiple modes:
//! - `run` (default): ACP stdio mode for use with bridge or ACP clients
//! - `chat`: Interactive REPL with slash commands
//! - `workspace`: Workspace management commands
//! - `config`: Configuration management

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::RwLock;
use tracing::{error, info};
use uuid::Uuid;

use agent_core::agent_loop::{AgentLoopConfig, ToolExecutor};
use agent_core::config::{self, AgentConfig};
use agent_core::plugin::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult};
use agent_core::session::SessionManager;
use agent_core::system_prompt::{SystemPromptManager, DEFAULT_SYSTEM_PROMPT};
use agent_core::transport::{IncomingMessage, JsonRpcId, StdioTransport, TransportSender};
use agent_core::{AgentBuilder, AgentRuntime};

use agent_client_protocol_schema::{
    AgentCapabilities, ContentBlock, Implementation, InitializeResponse, NewSessionResponse,
    PromptRequest, PromptResponse, ProtocolVersion, StopReason as AcpStopReason,
};

use agent_mcp_bridge::McpBridge;
use agent_provider_claude::ClaudeProvider;
use agent_provider_gemini::GeminiProvider;
use agent_provider_openai::OpenAiProvider;
use agent_storage_fs::{FsBindingStore, FsSessionStore, FsWorkspaceStore};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "aptove", version = "0.1.0", about = "Aptove — ACP AI Coding Agent")]
struct Cli {
    /// Path to config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Target workspace UUID (default: auto-detect via device binding)
    #[arg(long, global = true)]
    workspace: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start in ACP stdio mode (default)
    Run,
    /// Interactive REPL chat mode
    Chat,
    /// Workspace management
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// List all workspaces
    List,
    /// Create a new workspace
    Create {
        /// Optional workspace name
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a workspace
    Delete {
        /// Workspace UUID
        uuid: String,
    },
    /// Show workspace details
    Show {
        /// Workspace UUID
        uuid: String,
    },
    /// Garbage-collect stale workspaces
    Gc {
        /// Maximum age in days (default: 90)
        #[arg(long, default_value = "90")]
        max_age: u64,
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
async fn main() {
    // All logging goes to stderr (stdout is for JSON-RPC).
    // Disable ANSI color codes when stderr is not a real terminal
    // (e.g. when captured by the bridge) to avoid raw \x1b[…] in logs.
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(is_tty)
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = run().await {
        // Print the full error chain for clear diagnostics
        eprintln!("❌ Aptove fatal error: {}", e);
        for cause in e.chain().skip(1) {
            eprintln!("   caused by: {}", cause);
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Load config
    let agent_config = if let Some(ref path) = cli.config {
        AgentConfig::load_from(path)
            .map_err(|e| anyhow::anyhow!("failed to load config from '{}': {}", path.display(), e))?
    } else {
        AgentConfig::load_default()?
    };

    match cli.command.unwrap_or(Commands::Run) {
        Commands::Run => run_acp_mode(agent_config).await,
        Commands::Chat => run_chat_mode(agent_config).await,
        Commands::Workspace { action } => run_workspace_command(action, agent_config).await,
        Commands::Config { action } => run_config_command(action, agent_config),
    }
}

// ---------------------------------------------------------------------------
// Runtime construction
// ---------------------------------------------------------------------------

async fn build_runtime(config: AgentConfig) -> Result<AgentRuntime> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir.display()))?;

    info!(data_dir = %data_dir.display(), "using data directory");

    let mut builder = AgentBuilder::new(config.clone());
    let mut registered_providers: Vec<String> = Vec::new();

    // -- Register LLM providers based on config --
    if let Some(api_key) = config.resolve_api_key("claude") {
        let model = config.model_for_provider("claude");
        let base_url = config
            .providers
            .claude
            .as_ref()
            .and_then(|p| p.base_url.as_deref());
        info!(model = %model, "registering Claude provider");
        builder = builder.with_llm(Arc::new(ClaudeProvider::new(&api_key, &model, base_url)));
        registered_providers.push("claude".to_string());
    }
    if let Some(api_key) = config.resolve_api_key("gemini") {
        let model = config.model_for_provider("gemini");
        info!(model = %model, "registering Gemini provider");
        builder = builder.with_llm(Arc::new(GeminiProvider::new(&api_key, &model)));
        registered_providers.push("gemini".to_string());
    }
    if let Some(api_key) = config.resolve_api_key("openai") {
        let model = config.model_for_provider("openai");
        let base_url = config
            .providers
            .openai
            .as_ref()
            .and_then(|p| p.base_url.as_deref());
        info!(model = %model, "registering OpenAI provider");
        builder = builder.with_llm(Arc::new(OpenAiProvider::new(&api_key, &model, base_url)));
        registered_providers.push("openai".to_string());
    }

    if registered_providers.is_empty() {
        anyhow::bail!(
            "no LLM providers could be registered — no API keys found.\n\
             Set one of these environment variables:\n\
             • ANTHROPIC_API_KEY  (for Claude)\n\
             • GOOGLE_API_KEY    (for Gemini)\n\
             • OPENAI_API_KEY    (for OpenAI)\n\
             Or add api_key under [providers.<name>] in config.toml.\n\
             Config location: {}",
            AgentConfig::default_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into())
        );
    }

    info!(providers = ?registered_providers, active = %config.provider, "LLM providers registered");

    // Set active provider from config
    if !registered_providers.contains(&config.provider) {
        anyhow::bail!(
            "active provider '{}' has no API key configured.\n\
             Registered providers: [{}].\n\
             Either set the API key for '{}' or change the active provider in config.toml.",
            config.provider,
            registered_providers.join(", "),
            config.provider,
        );
    }
    builder = builder.with_active_provider(&config.provider);

    // -- Register storage backends --
    let ws_store = Arc::new(FsWorkspaceStore::new(&data_dir)
        .context("failed to initialize workspace store")?);
    let session_store = Arc::new(FsSessionStore::new(&data_dir)
        .context("failed to initialize session store")?);
    let binding_store = Arc::new(FsBindingStore::new_sync(&data_dir)
        .context("failed to initialize binding store")?);

    builder = builder
        .with_workspace_store(ws_store)
        .with_session_store(session_store)
        .with_binding_store(binding_store);

    builder.build()
        .context("failed to build agent runtime")
}

// ---------------------------------------------------------------------------
// MCP Bridge setup
// ---------------------------------------------------------------------------

async fn setup_mcp_bridge(
    config: &AgentConfig,
) -> Result<Option<Arc<tokio::sync::Mutex<McpBridge>>>> {
    // Filter to only servers whose command actually exists on PATH.
    // validate() already warned the user about missing ones.
    let available: Vec<_> = config
        .mcp_servers
        .iter()
        .filter(|s| which::which(&s.command).is_ok())
        .cloned()
        .collect();

    if available.is_empty() {
        return Ok(None);
    }

    let mut bridge = McpBridge::new();
    bridge.connect_all(&available).await?;
    Ok(Some(Arc::new(tokio::sync::Mutex::new(bridge))))
}

// ---------------------------------------------------------------------------
// ACP stdio mode
// ---------------------------------------------------------------------------

async fn run_acp_mode(config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in ACP mode");

    // Validate config — provide clear error on failure.
    // Note: validate() checks that the active provider has an API key.
    // If no config file was found (defaults used), this will fail because
    // defaults have no API keys. The error message tells the user what to do.
    match config.validate() {
        Ok(warnings) => {
            for w in &warnings {
                tracing::warn!("⚠️  {}", w);
            }
        }
        Err(e) => {
            error!("configuration validation failed: {:#}", e);
            anyhow::bail!("{}", e);
        }
    }

    // Build runtime — this can fail if no providers/storage available
    let mut runtime = build_runtime(config.clone()).await
        .context("failed to initialize agent runtime")?;

    info!("agent runtime initialized successfully");

    // Set up MCP bridge (once — used for both tool registration and state)
    let mcp_bridge = match setup_mcp_bridge(&config).await {
        Ok(Some(bridge)) => {
            let b = bridge.lock().await;
            let tool_count = b.all_tools().len();
            runtime.register_tools(b.all_tools());
            info!(tool_count, "MCP bridge connected, tools registered");
            drop(b);
            Some(bridge)
        }
        Ok(None) => {
            info!("no MCP servers configured");
            None
        }
        Err(e) => {
            // MCP bridge failure is non-fatal — agent works without tools
            error!(err = %e, "failed to set up MCP bridge (continuing without tools)");
            None
        }
    };

    let session_manager = SessionManager::new();
    let mut transport = StdioTransport::new();
    let sender = transport.sender();

    let state = Arc::new(AgentState {
        config: config.clone(),
        runtime: RwLock::new(runtime),
        session_manager,
        session_workspaces: RwLock::new(HashMap::new()),
        mcp_bridge,
        sender,
    });

    info!("ready to accept ACP requests");

    // Message dispatch loop
    while let Some(msg) = transport.recv().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(msg, &state).await {
                // Log the full error chain
                error!(err = %e, "error handling message");
                for cause in e.chain().skip(1) {
                    error!("  caused by: {}", cause);
                }
            }
        });
    }

    info!("ACP transport closed, shutting down");
    state.runtime.read().await.shutdown_plugins().await?;
    Ok(())
}

/// Shared state for message handling.
struct AgentState {
    config: AgentConfig,
    runtime: RwLock<AgentRuntime>,
    session_manager: SessionManager,
    /// Maps session_id → workspace_uuid for persistence.
    session_workspaces: RwLock<HashMap<String, String>>,
    mcp_bridge: Option<Arc<tokio::sync::Mutex<McpBridge>>>,
    sender: TransportSender,
}

// ---------------------------------------------------------------------------
// Message dispatch
// ---------------------------------------------------------------------------

async fn handle_message(msg: IncomingMessage, state: &AgentState) -> Result<()> {
    match msg {
        IncomingMessage::Request { id, method, params } => {
            let result = match method.as_str() {
                "initialize" => handle_initialize(id.clone(), state).await,
                "session/new" => handle_session_new(id.clone(), &params, state).await,
                "session/prompt" => handle_session_prompt(id.clone(), &params, state).await,
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

            // If a request handler returned an error, send it back as a
            // JSON-RPC error response so the client knows what happened
            if let Err(ref e) = result {
                let error_msg = format!("{:#}", e); // full error chain
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
            } else {
                tracing::debug!(method = %method, "unhandled notification");
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// ACP request handlers
// ---------------------------------------------------------------------------

async fn handle_initialize(id: JsonRpcId, state: &AgentState) -> Result<()> {
    let response = InitializeResponse::new(ProtocolVersion::LATEST)
        .agent_info(Implementation::new("Aptove", "0.1.0"))
        .agent_capabilities(AgentCapabilities::default());

    let result = serde_json::to_value(&response)
        .context("failed to serialize initialize response")?;
    state.sender.send_response(id, result).await
}

async fn handle_session_new(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let runtime = state.runtime.read().await;

    // Extract optional sessionId from _meta for session persistence
    let session_id_from_meta = params
        .as_ref()
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Extract optional device_id for workspace resolution (fallback if no sessionId)
    let device_id = params
        .as_ref()
        .and_then(|p| p.get("device_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    // For mobile apps: generate a session ID if not provided
    // This ensures workspace UUID and session ID match
    let session_id_to_use = if let Some(sid) = session_id_from_meta {
        sid
    } else if device_id == "default" {
        // Mobile app first connection (no stored sessionId yet)
        // Generate a new UUID that will be used for both workspace and session
        let new_id = Uuid::new_v4().to_string();
        info!(session_id = %new_id, "generated new session ID for mobile app");
        new_id
    } else {
        // Legacy device_id flow - will use device_id for workspace binding
        // Session will have separate random UUID
        String::new()
    };

    // Resolve workspace based on sessionId or device_id
    let (workspace, is_reusing_session) = if !session_id_to_use.is_empty() {
        // Mobile app - use sessionId as workspace UUID
        info!(session_id = %session_id_to_use, "using sessionId as workspace UUID");

        // Try to load existing workspace with this UUID
        match runtime.workspace_manager().workspace_store().load(&session_id_to_use).await {
            Ok(ws) => {
                // Workspace exists - this is a reconnection, mark as reusing
                info!(workspace = %ws.uuid, "loaded existing workspace for sessionId (reusing session)");
                (ws, true)
            }
            Err(_) => {
                // Workspace doesn't exist, create it with this UUID
                info!(session_id = %session_id_to_use, "creating new workspace with sessionId as UUID");
                let ws = runtime.workspace_manager()
                    .workspace_store()
                    .create(&session_id_to_use, None, None)
                    .await
                    .with_context(|| format!("failed to create workspace with UUID '{}'", session_id_to_use))?;
                // First connection - not reusing
                (ws, false)
            }
        }
    } else {
        // Legacy device_id binding flow (not mobile app)
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

    // Use workspace config (or global fallback) for provider/model resolution
    let provider_name = workspace.provider.as_deref()
        .unwrap_or(ws_config.provider.as_str())
        .to_string();
    let model = ws_config.model_for_provider(&provider_name);

    // If reusing session, clear the conversation history
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

    // Load persisted session (will be empty if we just cleared it)
    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace.uuid)
        .await
        .unwrap_or_else(|_| agent_core::persistence::SessionData::new(&provider_name, &model));

    // Get max tokens from provider
    let max_tokens = match runtime.active_provider() {
        Ok(p) => p.model_info().max_context_tokens,
        Err(_) => 200_000,
    };

    // Create in-memory session using the determined session ID
    let session_id = if !session_id_to_use.is_empty() {
        // Mobile app: use the session_id_to_use (either from _meta or newly generated)
        state
            .session_manager
            .create_session_with_id(session_id_to_use, max_tokens, &provider_name, &model)
            .await
    } else {
        // Legacy flow: generate random session ID
        state
            .session_manager
            .create_session(max_tokens, &provider_name, &model)
            .await
    };

    // Track session → workspace mapping
    state
        .session_workspaces
        .write()
        .await
        .insert(session_id.clone(), workspace.uuid.clone());

    // Inject system prompt and persisted messages
    let session_arc = state.session_manager.get_session(&session_id).await?;
    {
        let mut session = session_arc.lock().await;

        // System prompt
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

        // Restore persisted messages
        for msg in &session_data.messages {
            session.context.add_message(msg.clone());
        }
    }

    let result = serde_json::to_value(&NewSessionResponse::new(session_id))
        .context("failed to serialize session/new response")?;
    state.sender.send_response(id, result).await
}

async fn handle_session_prompt(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    // Parse the ACP PromptRequest using the official SDK type
    let prompt_req: PromptRequest = params
        .as_ref()
        .map(|p| serde_json::from_value::<PromptRequest>(p.clone()))
        .transpose()
        .context("failed to parse session/prompt request")?
        .context("session/prompt requires parameters")?;

    let session_id = prompt_req.session_id.0.to_string();

    // Extract text from the prompt content blocks
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

    // Lookup workspace for this session
    let workspace_uuid = state
        .session_workspaces
        .read()
        .await
        .get(&session_id)
        .cloned();

    let sender = state.sender.clone();
    let session_arc = state.session_manager.get_session(&session_id).await
        .with_context(|| format!("session '{}' not found", session_id))?;

    // Add user message
    {
        let mut session = session_arc.lock().await;
        session.add_user_message(&prompt_text);
        session.reset_cancel();
        session.is_busy = true;
    }

    // Persist user message
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

    // Get provider, tools, cancel token
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
    };

    // Build tool executor from MCP bridge
    let tool_executor: Option<ToolExecutor> = state.mcp_bridge.as_ref().map(|bridge| {
        let bridge = bridge.clone();
        Arc::new(move |req: ToolCallRequest| {
            let bridge = bridge.clone();
            Box::pin(async move {
                let b = bridge.lock().await;
                b.execute_tool(req).await
            }) as futures::future::BoxFuture<'static, ToolCallResult>
        }) as ToolExecutor
    });

    // Drop the runtime read lock before the long-running loop
    drop(runtime);

    // Run agent loop
    let result = agent_core::agent_loop::run_agent_loop(
        provider,
        &messages,
        &tools,
        tool_executor,
        &loop_config,
        cancel_token,
        Some(sender.clone()),
        &session_id,
        None, // TODO: pass PluginHost once wired into runtime
    )
    .await;

    match result {
        Ok(loop_result) => {
            // Update in-memory session
            {
                let mut session = session_arc.lock().await;
                for msg in &loop_result.new_messages {
                    session.context.add_message(msg.clone());
                }
                session.is_busy = false;
            }

            // Persist assistant messages (only text messages via SessionStore)
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
                agent_core::StopReason::EndTurn => AcpStopReason::EndTurn,
                agent_core::StopReason::ToolUse => AcpStopReason::EndTurn,
                agent_core::StopReason::MaxTokens => AcpStopReason::MaxTokens,
                agent_core::StopReason::StopSequence => AcpStopReason::EndTurn,
                agent_core::StopReason::Error => AcpStopReason::Cancelled,
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

    let runtime = build_runtime(config.clone()).await?;
    let provider_name = runtime.active_provider_name().to_string();
    let model = config.model_for_provider(&provider_name);

    // Load default workspace
    let workspace = runtime.workspace_manager().default().await?;

    // Set up MCP bridge for tools
    let mcp_bridge = setup_mcp_bridge(&config).await?;
    let tools: Vec<agent_core::plugin::ToolDefinition> = match &mcp_bridge {
        Some(b) => b.lock().await.all_tools(),
        None => Vec::new(),
    };

    // Build tool executor from MCP bridge
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

    // Load persisted session
    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace.uuid)
        .await
        .unwrap_or_else(|_| agent_core::persistence::SessionData::new(&provider_name, &model));

    // Build initial context (system prompt + persisted messages)
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

    let provider = runtime.active_provider()
        .context("no active LLM provider")?;
    let loop_config = AgentLoopConfig {
        max_iterations: config.agent.max_tool_iterations,
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

        // Handle slash commands
        if line.starts_with('/') {
            match line.as_str() {
                "/quit" | "/exit" | "/q" => {
                    eprintln!("Goodbye!");
                    break;
                }
                "/help" | "/h" => {
                    eprintln!("Available commands:");
                    eprintln!("  /clear      - Clear conversation and session.md");
                    eprintln!("  /context    - Show context window status");
                    eprintln!("  /workspace  - Show current workspace");
                    eprintln!("  /help       - Show this help");
                    eprintln!("  /quit       - Exit");
                }
                "/workspace" => {
                    eprintln!(
                        "📁 Workspace: {} ({})",
                        workspace.name.as_deref().unwrap_or("unnamed"),
                        workspace.uuid
                    );
                    eprintln!("   Provider: {} | Model: {}", provider_name, model);
                    eprintln!("   Messages: {}", messages.len() - 1); // exclude system prompt
                }
                "/clear" => {
                    // Clear persisted session.md
                    if let Err(e) = runtime
                        .workspace_manager()
                        .session_store()
                        .clear(&workspace.uuid)
                        .await
                    {
                        eprintln!("⚠ Failed to clear session file: {}", e);
                    }
                    // Reset in-memory messages to just the system prompt
                    messages.truncate(1);
                    eprintln!("🧹 Conversation cleared (session.md reset).");
                }
                "/context" => {
                    let msg_count = messages.len() - 1; // exclude system prompt
                    eprintln!("📊 Context: {} messages ({} user + assistant turns)", msg_count, msg_count / 2);
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

        // Add user message
        let user_msg = Message {
            role: Role::User,
            content: MessageContent::Text(line.clone()),
        };
        messages.push(user_msg.clone());

        // Persist user message
        let _ = runtime
            .workspace_manager()
            .session_store()
            .append(&workspace.uuid, &user_msg)
            .await;

        // Run agent loop
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let result = agent_core::agent_loop::run_agent_loop(
            provider.clone(),
            &messages,
            &tools,
            tool_executor.clone(),
            &loop_config,
            cancel_token,
            None, // no transport sender — chat mode prints directly
            "chat",
            None, // TODO: pass PluginHost once wired into runtime
        )
        .await;

        match result {
            Ok(loop_result) => {
                // Print and persist assistant messages
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

                    // Add to in-memory context
                    messages.push(msg.clone());

                    // Persist to session.md (SessionStore skips non-text messages)
                    let _ = runtime
                        .workspace_manager()
                        .session_store()
                        .append(&workspace.uuid, msg)
                        .await;
                }

                // Show token usage
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

    // Shutdown plugins
    runtime.shutdown_plugins().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Workspace commands
// ---------------------------------------------------------------------------

async fn run_workspace_command(action: WorkspaceAction, config: AgentConfig) -> Result<()> {
    let runtime = build_runtime(config).await?;
    let wm = runtime.workspace_manager();

    match action {
        WorkspaceAction::List => {
            let workspaces = wm.list().await?;
            if workspaces.is_empty() {
                println!("No workspaces found.");
            } else {
                println!(
                    "{:<38} {:<20} {:<24} {}",
                    "UUID", "NAME", "LAST ACCESSED", "PROVIDER"
                );
                println!("{}", "-".repeat(100));
                for ws in workspaces {
                    println!(
                        "{:<38} {:<20} {:<24} {}",
                        ws.uuid,
                        ws.name.as_deref().unwrap_or("-"),
                        ws.last_accessed.format("%Y-%m-%d %H:%M:%S"),
                        ws.provider.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        WorkspaceAction::Create { name } => {
            let ws = wm.create(name.as_deref(), None).await?;
            println!("✅ Created workspace: {}", ws.uuid);
            if let Some(ref name) = ws.name {
                println!("   Name: {}", name);
            }
        }
        WorkspaceAction::Delete { uuid } => {
            wm.delete(&uuid).await?;
            println!("🗑  Deleted workspace: {}", uuid);
        }
        WorkspaceAction::Show { uuid } => {
            let ws = wm.load(&uuid).await?;
            println!("Workspace: {}", ws.uuid);
            println!(
                "  Name:          {}",
                ws.name.as_deref().unwrap_or("-")
            );
            println!(
                "  Provider:      {}",
                ws.provider.as_deref().unwrap_or("-")
            );
            println!(
                "  Created:       {}",
                ws.created_at.format("%Y-%m-%d %H:%M:%S")
            );
            println!(
                "  Last Accessed: {}",
                ws.last_accessed.format("%Y-%m-%d %H:%M:%S")
            );
        }
        WorkspaceAction::Gc { max_age } => {
            let removed = wm.gc(max_age).await?;
            println!(
                "🧹 Removed {} stale workspace(s) (older than {} days)",
                removed, max_age
            );
        }
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
