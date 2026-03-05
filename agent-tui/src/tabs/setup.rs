//! Setup tab — Tailscale and Cloudflare panels.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tokio::sync::{mpsc, RwLock};

use crate::app::{AppCommand, AppState};
use crate::services::tailscale::TailscaleStatus;
use crate::services::cloudflare::CloudflareStatus;

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    render_tailscale_panel(frame, chunks[0], state);
    render_cloudflare_panel(frame, chunks[1], state);
}

fn render_tailscale_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    let (status_text, status_color, detail_lines) = match &state.tailscale_status {
        TailscaleStatus::Connected { ip, hostname } => (
            "Connected",
            Color::Green,
            vec![
                format!("IP:       {}", ip),
                format!("Hostname: {}", hostname),
            ],
        ),
        TailscaleStatus::NeedsLogin { auth_url } => (
            "Needs Login",
            Color::Yellow,
            vec![
                "Auth URL:".to_string(),
                auth_url.clone(),
            ],
        ),
        TailscaleStatus::NotRunning => (
            "Not Running",
            Color::Red,
            vec!["Press [t] to start Tailscale".to_string()],
        ),
        TailscaleStatus::NotInstalled => (
            "Not Installed",
            Color::DarkGray,
            vec!["Install tailscale to use this transport".to_string()],
        ),
        TailscaleStatus::Unknown => (
            "Checking...",
            Color::DarkGray,
            vec![],
        ),
    };

    let enabled = state.config.state.tailscale_enabled;
    let toggle_hint = if enabled { "[t] Disable" } else { "[t] Enable" };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(status_text, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];

    for detail in &detail_lines {
        lines.push(Line::from(detail.as_str()));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(
        Span::styled(toggle_hint, Style::default().fg(Color::Cyan))
    ));

    let title = if enabled { " Tailscale (●) " } else { " Tailscale (○) " };
    let panel = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

fn render_cloudflare_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    let (status_text, status_color, detail_lines) = match &state.cloudflare_status {
        CloudflareStatus::Running { hostname } => (
            "Running",
            Color::Green,
            vec![format!("Hostname: {}", hostname)],
        ),
        CloudflareStatus::Stopped => (
            "Stopped",
            Color::Red,
            vec!["Press [c] to start Cloudflare tunnel".to_string()],
        ),
        CloudflareStatus::Error(msg) => (
            "Error",
            Color::Red,
            vec![msg.clone()],
        ),
        CloudflareStatus::NotConfigured => (
            "Not Configured",
            Color::DarkGray,
            vec!["Add [transports.cloudflare] to config.toml".to_string()],
        ),
        CloudflareStatus::Unknown => (
            "Checking...",
            Color::DarkGray,
            vec![],
        ),
    };

    let enabled = state.config.state.cloudflare_enabled;
    let toggle_hint = if enabled { "[c] Disable" } else { "[c] Enable" };

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw("Status: "),
            Span::styled(status_text, Style::default().fg(status_color).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(""),
    ];
    for detail in &detail_lines {
        lines.push(Line::from(detail.as_str()));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        Span::styled(toggle_hint, Style::default().fg(Color::Cyan))
    ));

    let title = if enabled { " Cloudflare (●) " } else { " Cloudflare (○) " };
    let panel = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub async fn handle_key(
    key: KeyEvent,
    state: &Arc<RwLock<AppState>>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<()> {
    match key.code {
        KeyCode::Char('t') => {
            let _ = cmd_tx.send(AppCommand::ToggleTailscale).await;
        }
        KeyCode::Char('c') => {
            let _ = cmd_tx.send(AppCommand::ToggleCloudflare).await;
        }
        _ => {}
    }
    Ok(())
}
