//! Aptove Agent CLI
//!
//! Entry point. Default behaviour:
//! - `aptove`           → full-screen TUI (implemented in agent-tui crate)
//! - `aptove --headless`→ ACP bridge + scheduler without TUI
//! - `aptove --stdio`   → ACP stdio mode for use with an external bridge
//! - `aptove workspace` / `config` / `jobs` / `triggers` → management subcommands

mod commands;
mod dispatch;
mod handlers;
mod runtime;
mod state;
mod transport_lifecycle;

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
use agent_bridge::{BridgeServer, BridgeServeConfig};
use agent_tui;

use crate::commands::{Cli, Commands};
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
        eprintln!("❌ Aptove fatal error: {}", e);
        for cause in e.chain().skip(1) {
            eprintln!("   caused by: {}", cause);
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    // Load global config
    let base_config = if let Some(ref path) = cli.config {
        AgentConfig::load_from(path)
            .map_err(|e| anyhow::anyhow!("failed to load config from '{}': {}", path.display(), e))?
    } else {
        AgentConfig::load_default()?
    };

    // Merge workspace config overlay (strips identity/transports/state per ownership rules)
    let mut agent_config = if let Some(ref uuid) = cli.workspace {
        let ws_config_path = AgentConfig::data_dir()?
            .join("workspaces")
            .join(uuid)
            .join("config.toml");
        AgentConfig::load_with_workspace(Some(&ws_config_path))?
    } else {
        base_config
    };

    // Ensure identity (agent_id + auth_token) is populated; save if newly generated
    agent_config.ensure_identity();

    // Dispatch to appropriate mode
    match cli.command {
        Some(Commands::Stdio) | None if cli.stdio => run_acp_mode(agent_config).await,
        None if cli.headless => run_headless_mode(agent_config).await,
        None => run_tui_mode(agent_config).await,
        Some(Commands::Stdio) => run_acp_mode(agent_config).await,
        Some(Commands::Workspace { action }) => run_workspace_command(action, agent_config).await,
        Some(Commands::Config { action }) => run_config_command(action, agent_config),
        Some(Commands::Jobs { action }) => run_jobs_command(action, agent_config).await,
        Some(Commands::Triggers { action }) => run_triggers_command(action, agent_config).await,
    }
}

// ---------------------------------------------------------------------------
// TUI mode (default)
// ---------------------------------------------------------------------------

async fn run_tui_mode(config: AgentConfig) -> Result<()> {
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

    let runtime = build_runtime(config.clone()).await
        .context("failed to initialize agent runtime")?;

    agent_tui::run(config, runtime).await
}

// ---------------------------------------------------------------------------
// Headless mode — bridge + scheduler, no TUI (replaces old `aptove run`)
// ---------------------------------------------------------------------------

async fn run_headless_mode(mut config: AgentConfig) -> Result<()> {
    info!(provider = %config.provider, "starting Aptove in headless mode");

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

    // Start the daemon required by the active transport (Tailscale, etc.) and
    // wait for it to be ready before building the bridge listener.
    let _transport_guard = transport_lifecycle::prepare(&mut config).await
        .context("failed to prepare active transport")?;

    let mut runtime = build_runtime(config.clone()).await
        .context("failed to initialize agent runtime")?;

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
            error!(err = %e, "failed to set up MCP bridge (continuing without tools)");
            None
        }
    };

    // Start the background scheduler
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

    // Build bridge from AgentConfig transport settings
    let bridge_config = BridgeServeConfig {
        bind_addr: config.serve.bind_addr.clone(),
        advertise_addr: config.serve.advertise_addr.clone(),
        keep_alive: config.serve.keep_alive,
    };

    let mut server = BridgeServer::build_with_trigger_store(&config, &bridge_config, trigger_store.clone())
        .map_err(|e| {
            eprintln!("❌ Failed to start transport: {}", e);
            for cause in e.chain().skip(1) {
                eprintln!("   {}", cause);
            }
            std::process::exit(1);
        })
        .unwrap();

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
    _transport_guard.shutdown().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// ACP stdio mode
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
            error!(err = %e, "failed to set up MCP bridge (continuing without tools)");
            None
        }
    };

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
// Shared message dispatch loop
// ---------------------------------------------------------------------------

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
