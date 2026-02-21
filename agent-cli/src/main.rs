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
use tracing::{error, info, warn};
use uuid::Uuid;

use agent_core::agent_loop::{AgentLoopConfig, ToolExecutor};
use agent_core::config::{self, AgentConfig};
use agent_core::plugin::{Message, MessageContent, Role, ToolCallRequest, ToolCallResult};
use agent_core::scheduler::{JobDefinition, JobRun, JobStatus, JobSummary, SchedulerStore};
use agent_core::trigger::{TriggerDefinition, TriggerEvent, TriggerStore, TriggerSummary};
use agent_core::session::SessionManager;
use agent_core::system_prompt::{SystemPromptManager, DEFAULT_SYSTEM_PROMPT};
use agent_core::transport::{IncomingMessage, JsonRpcId, StdioTransport, TransportSender};
use agent_core::{AgentBuilder, AgentRuntime};

use agent_scheduler::{validate_cron, Scheduler};

use agent_client_protocol_schema::{
    AgentCapabilities, ContentBlock, Implementation, InitializeResponse, NewSessionResponse,
    PromptRequest, PromptResponse, ProtocolVersion, StopReason as AcpStopReason,
};

use agent_mcp_bridge::McpBridge;
use agent_bridge::{BridgeServer, BridgeServeConfig};
use agent_provider_claude::ClaudeProvider;
use agent_provider_gemini::GeminiProvider;
use agent_provider_openai::OpenAiProvider;
use agent_storage_fs::{FsBindingStore, FsSchedulerStore, FsSessionStore, FsTriggerStore, FsWorkspaceStore};

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
    /// Run the ACP agent and WebSocket bridge in a single process
    Serve {
        /// Port to listen on
        #[arg(long, default_value = "8765")]
        port: u16,
        /// Enable TLS
        #[arg(long, default_value = "true")]
        tls: bool,
        /// Bind address (default: 0.0.0.0)
        #[arg(long)]
        bind: Option<String>,
    },
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
    /// Scheduled job management
    Jobs {
        #[command(subcommand)]
        action: JobsAction,
    },
    /// Webhook trigger management
    Triggers {
        #[command(subcommand)]
        action: TriggersAction,
    },
}

#[derive(Subcommand)]
enum JobsAction {
    /// List all scheduled jobs in a workspace
    List {
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Create a new scheduled job
    Create {
        /// Job display name
        #[arg(long)]
        name: String,
        /// Prompt to send to the LLM on each run
        #[arg(long)]
        prompt: String,
        /// Cron expression (5-field, e.g. "0 9 * * *" for 9 AM daily)
        #[arg(long)]
        schedule: String,
        /// IANA timezone (e.g. "America/New_York"). Defaults to UTC.
        #[arg(long)]
        timezone: Option<String>,
        /// Run only once then disable
        #[arg(long)]
        one_shot: bool,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show a job's details and latest output
    Show {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Manually trigger a job immediately
    Run {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Enable a job
    Enable {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Disable a job
    Disable {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Delete a job and its run history
    Delete {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
enum TriggersAction {
    /// List all webhook triggers in a workspace
    List {
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Create a new webhook trigger
    Create {
        /// Trigger display name
        #[arg(long)]
        name: String,
        /// Prompt template (use {{payload}}, {{content_type}}, {{timestamp}}, {{trigger_name}}, {{headers}})
        #[arg(long)]
        prompt: String,
        /// Rate limit — max events per minute (0 = unlimited)
        #[arg(long, default_value = "60")]
        rate_limit: u32,
        /// Optional HMAC-SHA256 secret for signature verification
        #[arg(long)]
        hmac_secret: Option<String>,
        /// Accepted content types (comma-separated, empty = any)
        #[arg(long)]
        content_types: Option<String>,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show a trigger's details and latest run
    Show {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Send a test event to a trigger (executes the prompt template)
    Test {
        /// Trigger UUID
        trigger_id: String,
        /// Test payload body
        #[arg(long, default_value = "{}")]
        payload: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Enable a trigger
    Enable {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Disable a trigger
    Disable {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Delete a trigger and its run history
    Delete {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Regenerate the webhook token (invalidates the old URL)
    RegenerateToken {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
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
        Commands::Serve { port, tls, bind } => {
            let mut bridge_config = BridgeServeConfig::load()
                .unwrap_or_default();
            bridge_config.port = port;
            bridge_config.tls = tls;
            if let Some(addr) = bind {
                bridge_config.bind_addr = addr;
            }
            run_serve_mode(agent_config, bridge_config).await
        }
        Commands::Chat => run_chat_mode(agent_config).await,
        Commands::Workspace { action } => run_workspace_command(action, agent_config).await,
        Commands::Config { action } => run_config_command(action, agent_config),
        Commands::Jobs { action } => run_jobs_command(action, agent_config).await,
        Commands::Triggers { action } => run_triggers_command(action, agent_config).await,
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
    let scheduler_store = Arc::new(FsSchedulerStore::new(&data_dir)
        .context("failed to initialize scheduler store")?);
    let trigger_store = Arc::new(FsTriggerStore::new(&data_dir)
        .context("failed to initialize trigger store")?);

    builder = builder
        .with_workspace_store(ws_store)
        .with_session_store(session_store)
        .with_binding_store(binding_store)
        .with_scheduler_store(scheduler_store)
        .with_trigger_store(trigger_store);

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

    // Start the background scheduler (if scheduler store is configured)
    let _scheduler_handle = {
        let runtime_read = runtime.workspace_manager().workspace_store().clone();
        if let Some(store) = runtime.scheduler_store().cloned() {
            match runtime.active_provider() {
                Ok(provider) => {
                    let scheduler = Scheduler::new(store, runtime_read, provider);
                    let handle = scheduler.start();
                    info!("background scheduler started");
                    Some(handle)
                }
                Err(e) => {
                    warn!(err = %e, "could not start scheduler: no active provider");
                    None
                }
            }
        } else {
            None
        }
    };

    let scheduler_store = runtime.scheduler_store().cloned();
    let trigger_store = runtime.trigger_store().cloned();
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
        scheduler_store,
        trigger_store,
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

// ---------------------------------------------------------------------------
// Serve mode — embedded bridge + agent in a single process
// ---------------------------------------------------------------------------

async fn run_serve_mode(config: AgentConfig, bridge_config: BridgeServeConfig) -> Result<()> {
    info!(port = bridge_config.port, tls = bridge_config.tls, "starting Aptove in serve mode");

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

    let mut runtime = build_runtime(config.clone()).await
        .context("failed to initialize agent runtime")?;

    let mcp_bridge = match setup_mcp_bridge(&config).await {
        Ok(Some(bridge)) => {
            let b = bridge.lock().await;
            runtime.register_tools(b.all_tools());
            drop(b);
            Some(bridge)
        }
        Ok(None) => None,
        Err(e) => {
            error!(err = %e, "failed to set up MCP bridge (continuing without tools)");
            None
        }
    };

    // Start the background scheduler (if scheduler store is configured)
    let _scheduler_handle = {
        let ws_store = runtime.workspace_manager().workspace_store().clone();
        if let Some(store) = runtime.scheduler_store().cloned() {
            match runtime.active_provider() {
                Ok(provider) => {
                    let scheduler = Scheduler::new(store, ws_store, provider);
                    let handle = scheduler.start();
                    info!("background scheduler started");
                    Some(handle)
                }
                Err(e) => {
                    warn!(err = %e, "could not start scheduler: no active provider");
                    None
                }
            }
        } else {
            None
        }
    };

    let scheduler_store = runtime.scheduler_store().cloned();
    let trigger_store = runtime.trigger_store().cloned();
    let server = BridgeServer::build_with_trigger_store(&bridge_config, trigger_store.clone())?;
    let mut transport = server.transport;
    let sender = transport.sender();

    let state = Arc::new(AgentState {
        config: config.clone(),
        runtime: RwLock::new(runtime),
        session_manager: SessionManager::new(),
        session_workspaces: RwLock::new(HashMap::new()),
        mcp_bridge,
        sender,
        scheduler_store,
        trigger_store,
    });

    info!("ready — bridge server starting");

    let state_for_loop = state.clone();
    let agent_loop = async move {
        while let Some(msg) = transport.recv().await {
            let s = state_for_loop.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_message(msg, &s).await {
                    error!(err = %e, "error handling message");
                    for cause in e.chain().skip(1) {
                        error!("  caused by: {}", cause);
                    }
                }
            });
        }
        info!("agent transport closed");
        Ok::<_, anyhow::Error>(())
    };

    let bridge_serve = server.bridge.start();

    tokio::select! {
        res = agent_loop => { res?; }
        res = bridge_serve => { res.map_err(|e| anyhow::anyhow!("bridge error: {}", e))?; }
    }

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
    /// Scheduler store for jobs/* ACP methods (None if not configured).
    scheduler_store: Option<Arc<dyn SchedulerStore>>,
    /// Trigger store for triggers/* ACP methods (None if not configured).
    trigger_store: Option<Arc<dyn TriggerStore>>,
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
            } else if method == "triggers/execute" {
                // Fire-and-forget: bridge sends this when a webhook event arrives.
                // We deserialize the event and execute the trigger asynchronously.
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
                                        // Update last_event_at
                                        let mut updated = trigger.clone();
                                        updated.last_event_at = Some(event.received_at);
                                        updated.updated_at = chrono::Utc::now();
                                        tstore.update_trigger(&event.workspace_id, &updated).await.ok();
                                        // Prune old runs
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

async fn handle_session_load(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    // Accept "workspace_id" or "session_id" — in this architecture they are the same thing.
    let workspace_id = params
        .as_ref()
        .and_then(|p| p.get("workspace_id").or_else(|| p.get("session_id")))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("session/load requires a 'workspace_id' parameter"))?
        .to_string();

    let runtime = state.runtime.read().await;

    // Verify the workspace exists and update its last_accessed timestamp.
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

    // Apply workspace config overlay (same logic as session/new).
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

    // Load persisted session data.
    let session_data = runtime
        .workspace_manager()
        .session_store()
        .read(&workspace_id)
        .await
        .unwrap_or_else(|_| agent_core::persistence::SessionData::new(&provider_name, &model));

    info!(
        workspace = %workspace_id,
        message_count = session_data.messages.len(),
        provider = %session_data.frontmatter.provider,
        "restored session data"
    );

    // Get max context tokens from the active provider.
    let max_tokens = match runtime.active_provider() {
        Ok(p) => p.model_info().max_context_tokens,
        Err(_) => 200_000,
    };

    // Create (or replace) the in-memory session, using workspace_id as session_id
    // so subsequent session/prompt calls can reference it.
    let session_id = state
        .session_manager
        .create_session_with_id(workspace_id.clone(), max_tokens, &provider_name, &model)
        .await;

    // Track session → workspace mapping.
    state
        .session_workspaces
        .write()
        .await
        .insert(session_id.clone(), workspace_id.clone());

    // Inject system prompt and restore persisted messages.
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

// ---------------------------------------------------------------------------
// Jobs ACP handlers
// ---------------------------------------------------------------------------

/// Helper: get the scheduler store or return a JSON-RPC error.
macro_rules! require_scheduler_store {
    ($state:expr, $id:expr) => {
        match $state.scheduler_store.as_ref() {
            Some(s) => s.clone(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INTERNAL_ERROR,
                        "scheduler store not configured".to_string(),
                    )
                    .await;
            }
        }
    };
}

/// Helper: extract `workspace_id` from params or return a JSON-RPC error.
macro_rules! require_param_str {
    ($params:expr, $field:expr, $id:expr, $state:expr) => {
        match $params
            .as_ref()
            .and_then(|p| p.get($field))
            .and_then(|v| v.as_str())
        {
            Some(v) => v.to_string(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INVALID_PARAMS,
                        format!("missing required field: {}", $field),
                    )
                    .await;
            }
        }
    };
}

async fn handle_jobs_create(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let name = require_param_str!(params, "name", id, state);
    let prompt = require_param_str!(params, "prompt", id, state);
    let schedule = require_param_str!(params, "schedule", id, state);

    // Validate cron expression
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

async fn handle_jobs_list(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);

    let jobs = store.list_jobs(&workspace_id).await
        .context("failed to list jobs")?;

    let summaries: Vec<JobSummary> = jobs.iter().map(JobSummary::from).collect();
    let result = serde_json::to_value(&summaries)
        .context("failed to serialize job list")?;
    state.sender.send_response(id, result).await
}

async fn handle_jobs_get(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let job_id = require_param_str!(params, "job_id", id, state);

    let job = store.get_job(&workspace_id, &job_id).await
        .context("job not found")?;

    let result = serde_json::to_value(&job)
        .context("failed to serialize job")?;
    state.sender.send_response(id, result).await
}

async fn handle_jobs_update(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let job_id = require_param_str!(params, "job_id", id, state);

    // Load existing job and apply partial updates
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

async fn handle_jobs_delete(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let job_id = require_param_str!(params, "job_id", id, state);

    store.delete_job(&workspace_id, &job_id).await
        .context("failed to delete job")?;

    state.sender.send_response(id, serde_json::json!({})).await
}

async fn handle_jobs_history(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let job_id = require_param_str!(params, "job_id", id, state);

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

async fn handle_jobs_trigger(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_scheduler_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let job_id = require_param_str!(params, "job_id", id, state);

    let job = store.get_job(&workspace_id, &job_id).await
        .context("job not found")?;

    let provider = state.runtime.read().await.active_provider()
        .context("no active LLM provider")?;

    // Execute the job inline (manual trigger — same as scheduled execution)
    use std::time::Instant;
    let start = Instant::now();
    let timestamp = chrono::Utc::now();
    let run_id = Uuid::new_v4().to_string();

    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(job.prompt.clone()),
    }];
    let cancel = tokio_util::sync::CancellationToken::new();
    let loop_cfg = AgentLoopConfig { max_iterations: 25 };

    let run = match agent_core::agent_loop::run_agent_loop(
        provider,
        &messages,
        &[],
        None,
        &loop_cfg,
        cancel,
        None,
        &run_id,
        None,
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

    // Persist the run
    store.write_run(&workspace_id, &job.id, &run).await.ok();

    // Update last_run_at
    let mut updated_job = job.clone();
    updated_job.last_run_at = Some(run.timestamp);
    updated_job.updated_at = chrono::Utc::now();
    if job.one_shot && run.status == JobStatus::Success {
        updated_job.enabled = false;
    }
    store.update_job(&workspace_id, &updated_job).await.ok();

    // Prune old runs
    if job.max_history > 0 {
        store.prune_runs(&workspace_id, &job.id, job.max_history as usize).await.ok();
    }

    let result = serde_json::to_value(&run)
        .context("failed to serialize run result")?;
    state.sender.send_response(id, result).await
}

// ---------------------------------------------------------------------------
// Triggers ACP handlers
// ---------------------------------------------------------------------------

/// Helper: get the trigger store or return a JSON-RPC error.
macro_rules! require_trigger_store {
    ($state:expr, $id:expr) => {
        match $state.trigger_store.as_ref() {
            Some(s) => s.clone(),
            None => {
                return $state
                    .sender
                    .send_error(
                        $id,
                        agent_core::transport::error_codes::INTERNAL_ERROR,
                        "trigger store not configured".to_string(),
                    )
                    .await;
            }
        }
    };
}

async fn handle_triggers_create(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let name = require_param_str!(params, "name", id, state);
    let prompt_template = require_param_str!(params, "prompt_template", id, state);

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
        id: uuid::Uuid::new_v4().to_string(),
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

async fn handle_triggers_list(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);

    let triggers = store.list_triggers(&workspace_id).await
        .context("failed to list triggers")?;

    let summaries: Vec<TriggerSummary> = triggers.iter().map(TriggerSummary::from).collect();
    let result = serde_json::to_value(&summaries)
        .context("failed to serialize trigger list")?;
    state.sender.send_response(id, result).await
}

async fn handle_triggers_get(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let trigger_id = require_param_str!(params, "trigger_id", id, state);

    let trigger = store.get_trigger(&workspace_id, &trigger_id).await
        .context("trigger not found")?;

    let result = serde_json::to_value(&trigger)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
}

async fn handle_triggers_update(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let trigger_id = require_param_str!(params, "trigger_id", id, state);

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

async fn handle_triggers_delete(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let trigger_id = require_param_str!(params, "trigger_id", id, state);

    store.delete_trigger(&workspace_id, &trigger_id).await
        .context("failed to delete trigger")?;

    state.sender.send_response(id, serde_json::json!({})).await
}

async fn handle_triggers_history(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let trigger_id = require_param_str!(params, "trigger_id", id, state);

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

async fn handle_triggers_regenerate_token(
    id: JsonRpcId,
    params: &Option<serde_json::Value>,
    state: &AgentState,
) -> Result<()> {
    let store = require_trigger_store!(state, id);
    let workspace_id = require_param_str!(params, "workspace_id", id, state);
    let trigger_id = require_param_str!(params, "trigger_id", id, state);

    let trigger = store.regenerate_token(&workspace_id, &trigger_id).await
        .context("failed to regenerate token")?;

    let result = serde_json::to_value(&trigger)
        .context("failed to serialize trigger")?;
    state.sender.send_response(id, result).await
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

    let mut runtime = build_runtime(config.clone()).await?;
    let mut provider_name = runtime.active_provider_name().to_string();
    let mut model = config.model_for_provider(&provider_name);

    // Load default workspace
    let mut workspace = runtime.workspace_manager().default().await?;

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

    let mut provider = runtime.active_provider()
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
                                    agent_core::persistence::SessionData::new("-", "-")
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
                                        agent_core::persistence::SessionData::new(
                                            &provider_name,
                                            &model,
                                        )
                                    });

                                // Rebuild context: system prompt + restored messages.
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
                        // Re-create the provider with the new model name.
                        // build_runtime registered providers by name; we need to rebuild
                        // for the new model. Re-use current provider_name.
                        let new_runtime = build_runtime({
                            let mut c = config.clone();
                            // Override the model for the active provider
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

// ---------------------------------------------------------------------------
// Jobs CLI commands
// ---------------------------------------------------------------------------

async fn run_jobs_command(action: JobsAction, config: AgentConfig) -> Result<()> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    let store = Arc::new(
        FsSchedulerStore::new(&data_dir).context("failed to initialize scheduler store")?,
    );

    let runtime = build_runtime(config).await?;

    // Inline helper: resolve workspace UUID or fall back to default
    macro_rules! resolve_ws {
        ($opt:expr) => {
            if let Some(id) = $opt {
                id
            } else {
                runtime.workspace_manager().default().await?.uuid
            }
        };
    }

    match action {
        JobsAction::List { workspace } => {
            let ws_id = resolve_ws!(workspace);
            let jobs = store.list_jobs(&ws_id).await?;

            if jobs.is_empty() {
                println!("No scheduled jobs in workspace {}.", ws_id);
            } else {
                println!(
                    "{:<38} {:<24} {:<16} {:<10} {}",
                    "ID", "NAME", "SCHEDULE", "STATUS", "LAST RUN"
                );
                println!("{}", "-".repeat(110));
                for job in &jobs {
                    let status = if !job.enabled {
                        "disabled".to_string()
                    } else if job.last_run_at.is_none() {
                        "pending".to_string()
                    } else {
                        "enabled".to_string()
                    };
                    let last_run = job
                        .last_run_at
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<38} {:<24} {:<16} {:<10} {}",
                        job.id,
                        &job.name[..job.name.len().min(23)],
                        job.schedule,
                        status,
                        last_run,
                    );
                }
            }
        }

        JobsAction::Create {
            name,
            prompt,
            schedule,
            timezone,
            one_shot,
            workspace,
        } => {
            validate_cron(&schedule)
                .with_context(|| format!("invalid cron expression: {:?}", schedule))?;

            let ws_id = resolve_ws!(workspace);
            let now = chrono::Utc::now();
            let job = JobDefinition {
                id: Uuid::new_v4().to_string(),
                name: name.clone(),
                prompt,
                schedule: schedule.clone(),
                timezone,
                enabled: true,
                one_shot,
                notify: true,
                max_history: 7,
                created_at: now,
                updated_at: now,
                last_run_at: None,
            };

            let created = store.create_job(&ws_id, &job).await?;
            println!("✅ Created job: {}", created.id);
            println!("   Name:     {}", created.name);
            println!("   Schedule: {}", created.schedule);
            println!("   Workspace: {}", ws_id);
        }

        JobsAction::Show { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;

            println!("Job: {}", job.id);
            println!("  Name:      {}", job.name);
            println!("  Schedule:  {}", job.schedule);
            if let Some(tz) = &job.timezone {
                println!("  Timezone:  {}", tz);
            }
            println!("  Enabled:   {}", job.enabled);
            println!("  One-shot:  {}", job.one_shot);
            println!("  Notify:    {}", job.notify);
            println!("  Max runs:  {}", job.max_history);
            println!("  Created:   {}", job.created_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(t) = job.last_run_at {
                println!("  Last run:  {}", t.format("%Y-%m-%d %H:%M:%S"));
            }
            println!("\nPrompt:\n{}", job.prompt);

            // Show latest output if available
            let runs = store.list_runs(&ws_id, &job_id, 1).await.unwrap_or_default();
            if let Some(run) = runs.first() {
                println!("\nLatest run ({}):", run.timestamp.format("%Y-%m-%d %H:%M:%S"));
                println!("  Status: {}", run.status);
                if !run.output.is_empty() {
                    println!("\n{}", run.output);
                }
                if let Some(ref err) = run.error_message {
                    println!("  Error: {}", err);
                }
            }
        }

        JobsAction::Run { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;

            println!("Running job '{}' ({})...", job.name, job.id);

            let provider = runtime.active_provider()
                .context("no active LLM provider")?;

            let messages = vec![Message {
                role: Role::User,
                content: MessageContent::Text(job.prompt.clone()),
            }];
            let cancel = tokio_util::sync::CancellationToken::new();
            let loop_cfg = AgentLoopConfig { max_iterations: 25 };
            let start = std::time::Instant::now();
            let timestamp = chrono::Utc::now();
            let run_id = Uuid::new_v4().to_string();

            let run = match agent_core::agent_loop::run_agent_loop(
                provider,
                &messages,
                &[],
                None,
                &loop_cfg,
                cancel,
                None,
                &run_id,
                None,
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
                Err(e) => {
                    eprintln!("❌ Job execution failed: {}", e);
                    JobRun {
                        job_id: job.id.clone(),
                        timestamp,
                        status: JobStatus::Error,
                        output: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        error_message: Some(e.to_string()),
                    }
                }
            };

            store.write_run(&ws_id, &job.id, &run).await.ok();

            let mut updated_job = job.clone();
            updated_job.last_run_at = Some(run.timestamp);
            updated_job.updated_at = chrono::Utc::now();
            if job.one_shot && run.status == JobStatus::Success {
                updated_job.enabled = false;
            }
            store.update_job(&ws_id, &updated_job).await.ok();
            if job.max_history > 0 {
                store.prune_runs(&ws_id, &job.id, job.max_history as usize).await.ok();
            }

            println!("\nStatus: {} ({} ms)", run.status, run.duration_ms);
            if !run.output.is_empty() {
                println!("\n{}", run.output);
            }
            if let Some(ref err) = run.error_message {
                eprintln!("Error: {}", err);
            }
        }

        JobsAction::Enable { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            job.enabled = true;
            job.updated_at = chrono::Utc::now();
            store.update_job(&ws_id, &job).await?;
            println!("✅ Job '{}' enabled.", job.name);
        }

        JobsAction::Disable { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            job.enabled = false;
            job.updated_at = chrono::Utc::now();
            store.update_job(&ws_id, &job).await?;
            println!("Job '{}' disabled.", job.name);
        }

        JobsAction::Delete { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            store.delete_job(&ws_id, &job_id).await?;
            println!("🗑  Deleted job '{}' ({}).", job.name, job_id);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Triggers CLI commands
// ---------------------------------------------------------------------------

async fn run_triggers_command(action: TriggersAction, config: AgentConfig) -> Result<()> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    let store = Arc::new(
        FsTriggerStore::new(&data_dir).context("failed to initialize trigger store")?,
    );

    let runtime = build_runtime(config).await?;

    // Inline helper: resolve workspace UUID or fall back to default
    macro_rules! resolve_ws {
        ($opt:expr) => {
            if let Some(id) = $opt {
                id
            } else {
                runtime.workspace_manager().default().await?.uuid
            }
        };
    }

    match action {
        TriggersAction::List { workspace } => {
            let ws_id = resolve_ws!(workspace);
            let triggers = store.list_triggers(&ws_id).await?;

            if triggers.is_empty() {
                println!("No webhook triggers in workspace {}.", ws_id);
            } else {
                println!(
                    "{:<38} {:<24} {:<10} {}",
                    "ID", "NAME", "STATUS", "LAST EVENT"
                );
                println!("{}", "-".repeat(90));
                for t in &triggers {
                    let status = if t.enabled { "active" } else { "disabled" };
                    let last_event = t
                        .last_event_at
                        .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<38} {:<24} {:<10} {}",
                        t.id,
                        &t.name[..t.name.len().min(23)],
                        status,
                        last_event,
                    );
                }
            }
        }

        TriggersAction::Create {
            name,
            prompt,
            rate_limit,
            hmac_secret,
            content_types,
            workspace,
        } => {
            let ws_id = resolve_ws!(workspace);
            let accepted_content_types: Vec<String> = content_types
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let now = chrono::Utc::now();
            let trigger = TriggerDefinition {
                id: Uuid::new_v4().to_string(),
                name: name.clone(),
                prompt_template: prompt,
                token: agent_core::trigger::generate_token(),
                enabled: true,
                notify: true,
                max_history: 20,
                rate_limit_per_minute: rate_limit,
                hmac_secret,
                accepted_content_types,
                created_at: now,
                updated_at: now,
                last_event_at: None,
            };

            let created = store.create_trigger(&ws_id, &trigger).await?;
            println!("✅ Created trigger: {}", created.id);
            println!("   Name:      {}", created.name);
            println!("   Token:     {}", created.token);
            println!("   Workspace: {}", ws_id);
            println!("\nWebhook URL (when served): /webhook/{}", created.token);
        }

        TriggersAction::Show { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;

            println!("Trigger: {}", t.id);
            println!("  Name:          {}", t.name);
            println!("  Enabled:       {}", t.enabled);
            println!("  Notify:        {}", t.notify);
            println!("  Rate limit:    {}/min", t.rate_limit_per_minute);
            println!("  Max history:   {}", t.max_history);
            if let Some(ref s) = t.hmac_secret {
                println!("  HMAC secret:   {} (set)", s.chars().take(4).collect::<String>());
            }
            if !t.accepted_content_types.is_empty() {
                println!("  Content types: {}", t.accepted_content_types.join(", "));
            }
            println!("  Created:       {}", t.created_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(ts) = t.last_event_at {
                println!("  Last event:    {}", ts.format("%Y-%m-%d %H:%M:%S"));
            }
            println!("  Token:         {}", t.token);
            println!("\nPrompt template:\n{}", t.prompt_template);

            let runs = store.list_runs(&ws_id, &trigger_id, 1).await.unwrap_or_default();
            if let Some(run) = runs.first() {
                println!("\nLatest run ({}):", run.timestamp.format("%Y-%m-%d %H:%M:%S"));
                println!("  Status:  {}", run.status);
                println!("  Payload: {}", run.payload_preview);
                if !run.output.is_empty() {
                    println!("\n{}", run.output);
                }
                if let Some(ref err) = run.error_message {
                    println!("  Error: {}", err);
                }
            }
        }

        TriggersAction::Test { trigger_id, payload, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let trigger = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;

            println!("Testing trigger '{}' ({})...", trigger.name, trigger.id);

            let provider = runtime.active_provider()
                .context("no active LLM provider")?;

            let event = TriggerEvent {
                trigger_id: trigger.id.clone(),
                workspace_id: ws_id.clone(),
                payload: payload.clone(),
                content_type: "application/json".to_string(),
                headers: std::collections::HashMap::new(),
                received_at: chrono::Utc::now(),
            };

            // Use a placeholder SchedulerStore for the Scheduler constructor
            let sched_store = Arc::new(
                FsSchedulerStore::new(&data_dir).context("failed to initialize scheduler store")?
            );
            let ws_store = runtime.workspace_manager().workspace_store().clone();
            let scheduler = Scheduler::new(sched_store, ws_store, provider);
            let run = scheduler.execute_trigger(&trigger, &event).await;

            store.write_run(&ws_id, &trigger.id, &run).await.ok();

            println!("\nStatus: {} ({} ms)", run.status, run.duration_ms);
            if !run.output.is_empty() {
                println!("\n{}", run.output);
            }
            if let Some(ref err) = run.error_message {
                eprintln!("Error: {}", err);
            }
        }

        TriggersAction::Enable { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            t.enabled = true;
            t.updated_at = chrono::Utc::now();
            store.update_trigger(&ws_id, &t).await?;
            println!("✅ Trigger '{}' enabled.", t.name);
        }

        TriggersAction::Disable { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            t.enabled = false;
            t.updated_at = chrono::Utc::now();
            store.update_trigger(&ws_id, &t).await?;
            println!("Trigger '{}' disabled.", t.name);
        }

        TriggersAction::Delete { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            store.delete_trigger(&ws_id, &trigger_id).await?;
            println!("🗑  Deleted trigger '{}' ({}).", t.name, trigger_id);
        }

        TriggersAction::RegenerateToken { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.regenerate_token(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            println!("✅ Token regenerated for trigger '{}'.", t.name);
            println!("   New token: {}", t.token);
            println!("\nNew webhook URL (when served): /webhook/{}", t.token);
        }
    }

    Ok(())
}
