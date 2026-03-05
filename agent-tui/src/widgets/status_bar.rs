//! Bottom status bar widget.
//!
//! Displays: Tailscale status ● / ○ | Cloudflare status ● / ○ | Active provider

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::app::AppState;
use crate::services::tailscale::TailscaleStatus;
use crate::services::cloudflare::CloudflareStatus;

/// Render the single-row status bar at the bottom of the screen.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState) {
    let ts_span = match &state.tailscale_status {
        TailscaleStatus::Connected { ip, .. } => Span::styled(
            format!(" TS: ● {} ", ip),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        TailscaleStatus::NeedsLogin { .. } => Span::styled(
            " TS: ⚠ Login needed ",
            Style::default().fg(Color::Yellow),
        ),
        TailscaleStatus::NotRunning => Span::styled(
            " TS: ○ Off ",
            Style::default().fg(Color::DarkGray),
        ),
        TailscaleStatus::NotInstalled => Span::styled(
            " TS: ✗ N/A ",
            Style::default().fg(Color::DarkGray),
        ),
        TailscaleStatus::Unknown => Span::styled(
            " TS: … ",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let cf_span = match &state.cloudflare_status {
        CloudflareStatus::Running { .. } => Span::styled(
            " CF: ● On ",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        CloudflareStatus::Stopped => Span::styled(
            " CF: ○ Off ",
            Style::default().fg(Color::DarkGray),
        ),
        CloudflareStatus::Error(_) => Span::styled(
            " CF: ✗ Error ",
            Style::default().fg(Color::Red),
        ),
        CloudflareStatus::NotConfigured => Span::styled(
            " CF: - ",
            Style::default().fg(Color::DarkGray),
        ),
        CloudflareStatus::Unknown => Span::styled(
            " CF: … ",
            Style::default().fg(Color::DarkGray),
        ),
    };

    let provider_span = Span::styled(
        format!(" Provider: {} ", state.config.provider),
        Style::default().fg(Color::White),
    );

    let sep = Span::styled(" │ ", Style::default().fg(Color::DarkGray));

    let line = Line::from(vec![ts_span, sep.clone(), cf_span, sep, provider_span]);
    let bar = Paragraph::new(line)
        .style(Style::default().bg(Color::Black));
    frame.render_widget(bar, area);
}
