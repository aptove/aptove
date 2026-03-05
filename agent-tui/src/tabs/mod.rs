//! Tab definitions and top-level frame renderer.

pub mod setup;
pub mod chat;
pub mod connect;
pub mod jobs;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Tabs,
};

use crate::app::AppState;
use crate::widgets::status_bar;

/// The four TUI tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Setup,
    Chat,
    Connect,
    Jobs,
}

impl Tab {
    pub const ALL: &'static [Tab] = &[Tab::Setup, Tab::Chat, Tab::Connect, Tab::Jobs];

    pub fn name(self) -> &'static str {
        match self {
            Tab::Setup   => "Setup",
            Tab::Chat    => "Chat",
            Tab::Connect => "Connect",
            Tab::Jobs    => "Jobs",
        }
    }

    pub fn from_name(name: &str) -> Self {
        match name {
            "Chat"    => Tab::Chat,
            "Connect" => Tab::Connect,
            "Jobs"    => Tab::Jobs,
            _         => Tab::Setup,
        }
    }

    pub fn from_index(i: usize) -> Option<Self> {
        Tab::ALL.get(i).copied()
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        let i = (self.index() + 1) % Tab::ALL.len();
        Tab::ALL[i]
    }

    pub fn prev(self) -> Self {
        let i = (self.index() + Tab::ALL.len() - 1) % Tab::ALL.len();
        Tab::ALL[i]
    }
}

// ---------------------------------------------------------------------------
// Top-level frame renderer
// ---------------------------------------------------------------------------

/// Render the full terminal frame: tab bar + active tab content + status bar.
pub fn render_frame(frame: &mut Frame, state: &AppState, area: ratatui::layout::Rect) {

    // Layout: tab bar (3 rows) | content (fill) | status bar (1 row)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(area);

    // Tab bar
    let tab_titles: Vec<Line> = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let label = format!(" {} {} ", i + 1, t.name());
            Line::from(Span::styled(label, Style::default()))
        })
        .collect();

    let tabs_widget = Tabs::new(tab_titles)
        .select(state.active_tab.index())
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .title(format!(" Aptove v{} ", env!("CARGO_PKG_VERSION"))),
        );
    frame.render_widget(tabs_widget, chunks[0]);

    // Active tab content
    match state.active_tab {
        Tab::Setup   => setup::render(frame, chunks[1], state),
        Tab::Chat    => chat::render(frame, chunks[1], state),
        Tab::Connect => connect::render(frame, chunks[1], state),
        Tab::Jobs    => jobs::render(frame, chunks[1], state),
    }

    // Status bar
    status_bar::render(frame, chunks[2], state);

    // Quit confirmation overlay
    if state.quit_pending {
        render_quit_dialog(frame, area);
    }
}

fn render_quit_dialog(frame: &mut Frame, area: ratatui::layout::Rect) {
    use ratatui::{
        layout::Rect,
        widgets::{Block, Borders, Clear, Paragraph},
        text::Text,
    };
    let dialog = Rect {
        x: area.width / 2 - 20,
        y: area.height / 2 - 2,
        width: 40,
        height: 5,
    };
    frame.render_widget(Clear, dialog);
    let para = Paragraph::new(Text::from("\n Chat in progress. Quit? [y/n]"))
        .block(Block::default().borders(Borders::ALL).title(" Confirm Quit "));
    frame.render_widget(para, dialog);
}
