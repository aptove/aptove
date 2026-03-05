//! Jobs tab — scheduled job status and manual triggering.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table, TableState},
};
use tokio::sync::{mpsc, RwLock};

use crate::app::{AppCommand, AppState};

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let header = Row::new(vec![
        Cell::from("Name").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Schedule").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Status").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Last Run").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Next Run").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::Yellow));

    // TODO: populate from scheduler store in Phase 4
    let rows: Vec<Row> = Vec::new();

    let widths = [
        ratatui::layout::Constraint::Percentage(25),
        ratatui::layout::Constraint::Percentage(20),
        ratatui::layout::Constraint::Percentage(15),
        ratatui::layout::Constraint::Percentage(20),
        ratatui::layout::Constraint::Percentage(20),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Jobs — [↑↓] Select  [Enter] View output  [r] Trigger "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut table_state = TableState::default();
    frame.render_stateful_widget(table, area, &mut table_state);
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
        KeyCode::Char('r') => {
            // TODO: trigger selected job in Phase 4
            // let _ = cmd_tx.send(AppCommand::TriggerJob(selected_id)).await;
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Enter => {
            // TODO: job selection and popup in Phase 4
        }
        _ => {}
    }
    Ok(())
}
