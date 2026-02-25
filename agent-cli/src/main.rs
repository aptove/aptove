//! Aptove Agent CLI
//!
//! Binary entry point. Provides multiple modes:
//! - `run` (default): Standalone mode — ACP agent + WebSocket bridge in one process
//! - `stdio`: ACP stdio mode for use with an external bridge or ACP client
//! - `chat`: Interactive REPL with slash commands
//! - `workspace`: Workspace management commands
//! - `config`: Configuration management

mod commands;
mod dispatch;
mod handlers;
mod runtime;
mod state;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use agent_core::config::AgentConfig;
use agent_core::session::SessionManager;
use agent_core::transport::{StdioTransport, Transport};
use agent_scheduler::Scheduler;
use agent_bridge::{BridgeServer, BridgeServeConfig, CommonConfig};

use crate::commands::{Cli, Commands};
use crate::commands::chat::run_chat_mode;
use crate::commands::config::run_config_command;
use crate::commands::jobs::run_jobs_command;
use crate::commands::triggers::run_triggers_command;
use crate::commands::workspace::run_workspace_command;
use crate::dispatch::handle_message;
use crate::runtime::{build_runtime, setup_mcp_bridge};
use crate::state::AgentState;

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

    // Load global config (explicit path or default location)
    let base_config = if let Some(ref path) = cli.config {
        AgentConfig::load_from(path)
            .map_err(|e| anyhow::anyhow!("failed to load config from '{}': {}", path.display(), e))?
    } else {
        AgentConfig::load_default()?
    };

    // If a specific workspace is given, merge its config.toml on top of the global config.
    // This lets workspace/config.toml override any [serve] (or other) settings.
    let agent_config = if let Some(ref uuid) = cli.workspace {
        let ws_config_path = AgentConfig::data_dir()?
            .join("workspaces")
            .join(uuid)
            .join("config.toml");
        AgentConfig::load_with_workspace(Some(&ws_config_path))?
    } else {
        base_config
    };

    match cli.command.unwrap_or(Commands::Run {
        port: None, tls: None, bind: None,
    }) {
        Commands::Stdio => run_acp_mode(agent_config).await,
        _ if cli.stdio => run_acp_mode(agent_config).await,
        Commands::ShowQr => run_show_qr().await,
        Commands::Run { port, tls, bind } => {
            // Start from the [serve] section in config.toml (or workspace override).
            // CLI flags take precedence over config file values.
            // Transport selection is now read from common.toml via BridgeServeConfig::load().
            let serve = &agent_config.serve;
            let bridge_config = BridgeServeConfig {
                port: port.unwrap_or(serve.port),
                tls: tls.unwrap_or(serve.tls),
                bind_addr: bind.unwrap_or_else(|| serve.bind_addr.clone()),
                auth_token: serve.auth_token.clone(),
                keep_alive: serve.keep_alive,
                ..BridgeServeConfig::default()
            };
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
// Shared message dispatch loop
// ---------------------------------------------------------------------------

/// Drive a transport's receive loop, spawning a handler task per message.
async fn run_message_loop(transport: &mut dyn Transport, state: Arc<AgentState>) {
    while let Some(msg) = transport.recv().await {
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_message(msg, &state).await {
                error!(err = %e, "error handling message");
                for cause in e.chain().skip(1) {
                    error!("  caused by: {}", cause);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Show QR command (aptove show-qr)
// ---------------------------------------------------------------------------

async fn run_show_qr() -> Result<()> {
    // agent-bridge stores common.toml in the same dir as bridge.toml
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("aptove")
        .join("bridge");

    let mut config = CommonConfig::load_from_dir(&config_dir)?;
    config.ensure_agent_id();
    config.ensure_auth_token();
    config.save_to_dir(&config_dir)?;

    match agent_bridge::show_qr(&config_dir, &config)? {
        true => {}
        false => {
            eprintln!("Agent is not running. Start it with: aptove run");
            std::process::exit(1);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// ACP stdio mode (aptove stdio)
// ---------------------------------------------------------------------------

async fn run_acp_mode(config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in ACP mode");

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

    info!("agent runtime initialized successfully");

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
    let sender = transport.get_sender();

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

    run_message_loop(&mut transport, state.clone()).await;

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
    let mut server = BridgeServer::build_with_trigger_store(&bridge_config, trigger_store.clone())?;
    let mut transport = server.take_transport();
    let sender = transport.get_sender();

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
        run_message_loop(&mut transport, state_for_loop).await;
        info!("agent transport closed");
        Ok::<_, anyhow::Error>(())
    };

    let bridge_serve = server.start();

    tokio::select! {
        res = agent_loop => { res?; }
        res = bridge_serve => { res?; }
    }

    state.runtime.read().await.shutdown_plugins().await?;
    Ok(())
}
