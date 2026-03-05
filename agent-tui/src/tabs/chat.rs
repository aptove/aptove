//! Chat tab — scrollable message history + input box.

use std::sync::Arc;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::{mpsc, RwLock};

use crate::app::{AppCommand, AppState};

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    // 80% for history, 20% for input (min 3 rows)
    let input_height = 3u16;
    let history_height = area.height.saturating_sub(input_height);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(history_height),
            Constraint::Length(input_height),
        ])
        .split(area);

    render_history(frame, chunks[0], state);
    render_input(frame, chunks[1], state);
}

fn render_history(frame: &mut Frame, area: Rect, state: &AppState) {
    let items: Vec<ListItem> = state
        .chat_messages
        .iter()
        .map(|(role, content)| {
            let (prefix, style) = if role == "user" {
                ("You: ", Style::default().fg(Color::Cyan))
            } else {
                ("AI:  ", Style::default().fg(Color::Green))
            };
            let line = Line::from(vec![
                Span::styled(prefix, style.add_modifier(Modifier::BOLD)),
                Span::raw(content.as_str()),
            ]);
            ListItem::new(line)
        })
        .collect();

    let title = if state.chat_streaming {
        " Chat (streaming…) "
    } else {
        " Chat "
    };

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, area);
}

fn render_input(frame: &mut Frame, area: Rect, state: &AppState) {
    let hint = if state.chat_streaming {
        " [Esc] Cancel "
    } else {
        " [Enter] Send "
    };
    let input_text = format!("{}_", state.chat_input); // trailing cursor
    let para = Paragraph::new(input_text)
        .block(Block::default().borders(Borders::ALL).title(hint))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub async fn handle_key(
    key: KeyEvent,
    state: &Arc<RwLock<AppState>>,
    cmd_tx: &mpsc::Sender<AppCommand>,
) -> Result<()> {
    let mut st = state.write().await;

    match key.code {
        KeyCode::Esc => {
            if st.chat_streaming {
                drop(st);
                let _ = cmd_tx.send(AppCommand::CancelChat).await;
            }
        }
        KeyCode::Enter => {
            if !st.chat_streaming && !st.chat_input.is_empty() {
                let msg = st.chat_input.clone();
                drop(st);
                let _ = cmd_tx.send(AppCommand::SendChatMessage(msg)).await;
            }
        }
        KeyCode::Backspace => {
            st.chat_input.pop();
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+C handled globally
        }
        KeyCode::Up => {
            st.chat_scroll = st.chat_scroll.saturating_sub(1);
        }
        KeyCode::Down => {
            st.chat_scroll += 1;
        }
        KeyCode::Char(c) => {
            if !st.chat_streaming {
                st.chat_input.push(c);
            }
        }
        _ => {}
    }

    Ok(())
}
