//! Application state and main event loop.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch, RwLock};

use agent_core::builder::AgentRuntime;
use agent_core::config::AgentConfig;

use crate::services::bridge::BridgeService;
use crate::services::tailscale::{TailscaleService, TailscaleStatus};
use crate::services::cloudflare::{CloudflareService, CloudflareStatus};
use crate::tabs::Tab;

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// Shared application state — mutated by service tasks, read by the render loop.
pub struct AppState {
    /// Currently active tab.
    pub active_tab: Tab,
    /// Whether a quit confirmation dialog is pending.
    pub quit_pending: bool,
    /// Live Tailscale status.
    pub tailscale_status: TailscaleStatus,
    /// Live Cloudflare status.
    pub cloudflare_status: CloudflareStatus,
    /// Chat message history: `(role, content)` tuples.
    pub chat_messages: Vec<(String, String)>,
    /// Text currently being typed in the chat input box.
    pub chat_input: String,
    /// Whether an LLM response is currently streaming.
    pub chat_streaming: bool,
    /// Scroll offset in the chat message list.
    pub chat_scroll: usize,
    /// Active transport URLs (name → url).
    pub active_transports: Vec<(String, String)>,
    /// Connected clients (device_name, transport, connected_since).
    pub connected_clients: Vec<(String, String, String)>,
    /// The global AgentConfig.
    pub config: AgentConfig,
}

impl AppState {
    pub fn new(config: AgentConfig) -> Self {
        let last_tab = Tab::from_name(&config.state.last_tab);
        Self {
            active_tab: last_tab,
            quit_pending: false,
            tailscale_status: TailscaleStatus::Unknown,
            cloudflare_status: CloudflareStatus::Unknown,
            chat_messages: Vec::new(),
            chat_input: String::new(),
            chat_streaming: false,
            chat_scroll: 0,
            active_transports: Vec::new(),
            connected_clients: Vec::new(),
            config,
        }
    }

    pub fn next_tab(&mut self) {
        self.active_tab = self.active_tab.next();
    }

    pub fn prev_tab(&mut self) {
        self.active_tab = self.active_tab.prev();
    }

    pub fn go_to_tab(&mut self, n: usize) {
        if let Some(tab) = Tab::from_index(n) {
            self.active_tab = tab;
        }
    }
}

// ---------------------------------------------------------------------------
// Commands sent from the UI to service tasks
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum AppCommand {
    ToggleTailscale,
    ToggleCloudflare,
    /// Advance `state.active_transport` to the next enabled transport and
    /// persist the selection immediately to `config.toml`.
    CycleActiveTransport,
    SendChatMessage(String),
    CancelChat,
    TriggerJob(String),
    Quit,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the full-screen TUI.
pub async fn run_app(config: AgentConfig, _runtime: AgentRuntime) -> Result<()> {
    // Set up terminal
    terminal::enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    stdout.execute(EnterAlternateScreen)?;

    // Install panic hook to restore terminal on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = std::io::stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    let result = run_app_inner(config).await;

    // Restore terminal
    terminal::disable_raw_mode()?;
    stdout.execute(LeaveAlternateScreen)?;

    result
}

async fn run_app_inner(config: AgentConfig) -> Result<()> {
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let state = Arc::new(RwLock::new(AppState::new(config.clone())));

    // Command channel: UI → services
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<AppCommand>(32);

    // Tailscale status watch channel
    let (ts_tx, ts_rx) = watch::channel(TailscaleStatus::Unknown);

    // Cloudflare status watch channel
    let (cf_tx, cf_rx) = watch::channel(CloudflareStatus::Unknown);

    // Spawn Tailscale service
    let ts_config = config.clone();
    tokio::spawn(async move {
        let svc = TailscaleService::new(ts_config, ts_tx);
        svc.run().await;
    });

    // Spawn Cloudflare service
    let cf_config = config.clone();
    tokio::spawn(async move {
        let svc = CloudflareService::new(cf_config, cf_tx);
        svc.run().await;
    });

    // State-update task: apply watch channel updates into AppState
    let updater_state = state.clone();
    let mut ts_rx_clone = ts_rx.clone();
    let mut cf_rx_clone = cf_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Ok(()) = ts_rx_clone.changed() => {
                    let status = ts_rx_clone.borrow().clone();
                    updater_state.write().await.tailscale_status = status;
                }
                Ok(()) = cf_rx_clone.changed() => {
                    let status = cf_rx_clone.borrow().clone();
                    updater_state.write().await.cloudflare_status = status;
                }
            }
        }
    });

    // Bridge service: waits for active transport readiness, publishes URL
    let bridge_state = state.clone();
    let bridge_config = config.clone();
    let bridge_ts_rx = ts_rx.clone();
    let bridge_cf_rx = cf_rx.clone();
    tokio::spawn(async move {
        let svc = BridgeService::new(bridge_config, bridge_state, bridge_ts_rx, bridge_cf_rx);
        svc.run().await;
    });

    // Command handler task
    let cmd_state = state.clone();
    let cmd_config = config.clone();
    tokio::spawn(async move {
        handle_commands(&mut cmd_rx, cmd_state, cmd_config).await;
    });

    // Main render loop — 60ms tick
    let tick = Duration::from_millis(60);
    let mut should_quit = false;

    while !should_quit {
        // Draw current frame
        {
            let st = state.read().await;
            terminal.draw(|frame| {
                let area = frame.size();
                crate::tabs::render_frame(frame, &st, area);
            })?;
        }

        // Poll for keyboard events
        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                let quit = handle_key(key, &state, &cmd_tx).await?;
                if quit {
                    should_quit = true;
                }
            }
        }
    }

    // Persist state on clean exit
    {
        let st = state.read().await;
        let mut cfg = st.config.clone();
        cfg.state.last_tab = st.active_tab.name().to_string();
        cfg.state.tailscale_enabled = matches!(
            st.tailscale_status,
            TailscaleStatus::Connected { .. } | TailscaleStatus::NeedsLogin { .. }
        );
        cfg.state.cloudflare_enabled = matches!(st.cloudflare_status, CloudflareStatus::Running { .. });
        if let Err(e) = cfg.save() {
            tracing::warn!("failed to persist state on exit: {}", e);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

async fn handle_key(
    key: KeyEvent,
    state: &Arc<RwLock<AppState>>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<bool> {
    let mut st = state.write().await;

    // Quit confirmation dialog
    if st.quit_pending {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Char('q') => {
                return Ok(true);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                st.quit_pending = false;
            }
            _ => {}
        }
        return Ok(false);
    }

    // Global bindings
    match key.code {
        // Quit
        KeyCode::Char('q') if st.active_tab != Tab::Chat => {
            if st.chat_streaming {
                st.quit_pending = true;
            } else {
                return Ok(true);
            }
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return Ok(true);
        }
        // Tab cycling
        KeyCode::Tab => st.next_tab(),
        KeyCode::BackTab => st.prev_tab(),
        // Number key tab jumps
        KeyCode::Char('1') => st.go_to_tab(0),
        KeyCode::Char('2') => st.go_to_tab(1),
        KeyCode::Char('3') => st.go_to_tab(2),
        KeyCode::Char('4') => st.go_to_tab(3),
        // Tab-specific bindings
        _ => {
            let tab = st.active_tab;
            drop(st); // release lock before calling tab handler
            handle_tab_key(key, tab, state, cmd_tx).await?;
            return Ok(false);
        }
    }

    Ok(false)
}

async fn handle_tab_key(
    key: KeyEvent,
    tab: Tab,
    state: &Arc<RwLock<AppState>>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<()> {
    match tab {
        Tab::Setup => crate::tabs::setup::handle_key(key, state, cmd_tx).await,
        Tab::Chat => crate::tabs::chat::handle_key(key, state, cmd_tx).await,
        Tab::Connect => crate::tabs::connect::handle_key(key, state, cmd_tx).await,
        Tab::Jobs => crate::tabs::jobs::handle_key(key, state, cmd_tx).await,
    }
}

// ---------------------------------------------------------------------------
// Command handler
// ---------------------------------------------------------------------------

async fn handle_commands(
    rx: &mut mpsc::Receiver<AppCommand>,
    state: Arc<RwLock<AppState>>,
    _config: AgentConfig,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            AppCommand::ToggleTailscale => {
                let mut st = state.write().await;
                st.config.state.tailscale_enabled = !st.config.state.tailscale_enabled;
            }
            AppCommand::ToggleCloudflare => {
                let mut st = state.write().await;
                st.config.state.cloudflare_enabled = !st.config.state.cloudflare_enabled;
            }
            AppCommand::CycleActiveTransport => {
                let mut st = state.write().await;
                let all = &["local", "tailscale-serve", "tailscale-ip", "cloudflare"];
                let enabled: Vec<&str> = all
                    .iter()
                    .copied()
                    .filter(|&n| st.config.transports.enabled_names().contains(&n))
                    .collect();
                if !enabled.is_empty() {
                    let current = st.config.state.active_transport.clone();
                    let idx = enabled.iter().position(|&n| n == current.as_str()).unwrap_or(0);
                    let next = enabled[(idx + 1) % enabled.len()];
                    st.config.state.active_transport = next.to_string();
                    if let Err(e) = st.config.save() {
                        tracing::warn!("failed to save active_transport: {}", e);
                    }
                }
            }
            AppCommand::SendChatMessage(msg) => {
                let mut st = state.write().await;
                st.chat_messages.push(("user".to_string(), msg));
                st.chat_input.clear();
                st.chat_streaming = true;
                // TODO: spawn LLM request task in Phase 3
            }
            AppCommand::CancelChat => {
                let mut st = state.write().await;
                st.chat_streaming = false;
            }
            AppCommand::TriggerJob(_job_id) => {
                // TODO: implement in Phase 4
            }
            AppCommand::Quit => {
                // handled via return value in key handler
            }
        }
    }
}

