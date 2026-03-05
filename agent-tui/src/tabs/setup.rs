//! Setup tab — Tailscale, Cloudflare, and Active Transport panels.

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
    // Layout: top panels (fill) | transport selector (3 rows) | hint (1 row)
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    // Top: Tailscale (left) and Cloudflare (right)
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    render_tailscale_panel(frame, cols[0], state);
    render_cloudflare_panel(frame, cols[1], state);

    // Middle: active transport selector
    render_transport_selector(frame, rows[1], state);

    // Bottom: hint line
    render_hint_line(frame, rows[2], state);
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

fn render_transport_selector(frame: &mut Frame, area: Rect, state: &AppState) {
    let active = &state.config.state.active_transport;
    let enabled = state.config.transports.enabled_names();

    // Build spans: each transport option, highlighted if active
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw("  Active: "));

    let options = ["local", "tailscale-serve", "tailscale-ip", "cloudflare"];
    for (i, opt) in options.iter().enumerate() {
        let is_active = active == opt;
        let is_enabled = enabled.contains(opt);

        let label = match *opt {
            "local"           => "Local",
            "tailscale-serve" => "Tailscale (serve)",
            "tailscale-ip"    => "Tailscale (IP)",
            "cloudflare"      => "Cloudflare",
            _                 => opt,
        };

        let style = if is_active {
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)
        } else if is_enabled {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let display = format!(" {} ", label);
        spans.push(Span::styled(display, style));

        if i < options.len() - 1 {
            spans.push(Span::raw("  "));
        }
    }

    spans.push(Span::raw("   "));
    spans.push(Span::styled("[a] cycle", Style::default().fg(Color::Cyan)));

    let line = Line::from(spans);
    let para = Paragraph::new(line)
        .block(Block::default().borders(Borders::ALL).title(" Active Transport "));
    frame.render_widget(para, area);
}

fn render_hint_line(frame: &mut Frame, area: Rect, state: &AppState) {
    let active = &state.config.state.active_transport;
    let extra = match active.as_str() {
        "tailscale-serve" | "tailscale-ip" => "  [t] toggle Tailscale",
        "cloudflare"                        => "  [c] toggle Cloudflare",
        _                                   => "",
    };
    let text = format!("[t] Tailscale  [c] Cloudflare  [a] cycle transport{}", extra);
    let hint = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(hint, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub async fn handle_key(
    key: KeyEvent,
    _state: &Arc<RwLock<AppState>>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<()> {
    match key.code {
        KeyCode::Char('t') => {
            let _ = cmd_tx.send(AppCommand::ToggleTailscale).await;
        }
        KeyCode::Char('c') => {
            let _ = cmd_tx.send(AppCommand::ToggleCloudflare).await;
        }
        KeyCode::Char('a') => {
            let _ = cmd_tx.send(AppCommand::CycleActiveTransport).await;
        }
        _ => {}
    }
    Ok(())
}
