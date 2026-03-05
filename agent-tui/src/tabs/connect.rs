//! Connect tab — ASCII QR code and connected client list.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::{mpsc, RwLock};

use crate::app::{AppCommand, AppState};
use crate::widgets::qr;

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    render_qr_panel(frame, chunks[0], state);
    render_clients_panel(frame, chunks[1], state);
}

fn render_qr_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    let content = if state.active_transports.is_empty() {
        Paragraph::new("No active transports.\nEnable a transport in the Setup tab.")
            .block(Block::default().borders(Borders::ALL).title(" Connect "))
            .wrap(Wrap { trim: false })
    } else {
        // Show the first active transport URL as the QR content
        let url = &state.active_transports[0].1;
        let qr_text = qr::render_qr_text(url);

        let mut lines: Vec<Line> = qr_text
            .lines()
            .map(|l| Line::from(Span::raw(l.to_string())))
            .collect();

        lines.push(Line::from(""));
        for (name, url) in &state.active_transports {
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", name), Style::default().fg(Color::Cyan)),
                Span::raw(url.as_str()),
            ]));
        }

        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Connect — [r] Refresh "))
            .wrap(Wrap { trim: false })
    };

    frame.render_widget(content, area);
}

fn render_clients_panel(frame: &mut Frame, area: Rect, state: &AppState) {
    let items: Vec<ListItem> = if state.connected_clients.is_empty() {
        vec![ListItem::new("No clients connected")]
    } else {
        state
            .connected_clients
            .iter()
            .map(|(name, transport, since)| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{} ", name), Style::default().fg(Color::White)),
                    Span::styled(format!("[{}] ", transport), Style::default().fg(Color::Cyan)),
                    Span::styled(since.as_str(), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect()
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(" Connected Clients "));
    frame.render_widget(list, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub async fn handle_key(
    key: KeyEvent,
    _state: &Arc<RwLock<AppState>>,
    _cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<()> {
    match key.code {
        KeyCode::Char('r') => {
            // TODO: regenerate pairing code in Phase 3
        }
        _ => {}
    }
    Ok(())
}
