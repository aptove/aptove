use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub struct TransportState {
    pub name: String,
    pub addr: String,
    pub up: bool,
}

/// Current state of the push notification subsystem, shown in the status bar.
#[derive(Debug, Clone, PartialEq)]
pub enum PushStatus {
    /// Push relay not configured.
    Disabled,
    /// Last push delivery (test or real) succeeded.
    Ok,
    /// Last push delivery (test or real) failed.
    Error,
}

pub fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    version: &str,
    transports: &[TransportState],
    push_status: &PushStatus,
    keep_alive: bool,
    cwd: &str,
    update_available: Option<&str>,
) {
    let base = Style::default().bg(Color::DarkGray).fg(Color::White);
    let mut spans: Vec<Span> = vec![Span::styled(
        format!(" v{}", version),
        base,
    )];

    for t in transports {
        let dot_color = if t.up { Color::Green } else { Color::Red };
        spans.push(Span::styled(format!("  [{} ", t.name), base));
        spans.push(Span::styled("●", Style::default().bg(Color::DarkGray).fg(dot_color)));
        spans.push(Span::styled("]", base));
    }

    let push_dot_color = match push_status {
        PushStatus::Disabled => Color::DarkGray,
        PushStatus::Ok       => Color::Green,
        PushStatus::Error    => Color::Red,
    };
    spans.push(Span::styled("  [push ", base));
    spans.push(Span::styled("●", Style::default().bg(Color::DarkGray).fg(push_dot_color)));
    spans.push(Span::styled("]", base));

    let ka_color = if keep_alive { Color::Green } else { Color::Red };
    spans.push(Span::styled("  [keep alive ", base));
    spans.push(Span::styled("●", Style::default().bg(Color::DarkGray).fg(ka_color)));
    spans.push(Span::styled("]", base));

    // Build right-side spans: optional update indicator + cwd label.
    let mut right_spans: Vec<Span> = Vec::new();
    if let Some(v) = update_available {
        right_spans.push(Span::styled(
            format!(" \u{2191} New version v{} ", v),
            Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD),
        ));
        right_spans.push(Span::styled(
            "[Ctrl+U to update]  ",
            Style::default().bg(Color::DarkGray).fg(Color::Rgb(144, 238, 144)).add_modifier(Modifier::BOLD),
        ));
    }
    let cwd_text = format!("[path {}] ", cwd);
    right_spans.push(Span::styled(cwd_text, base));

    let right_width: u16 = right_spans.iter().map(|s| s.content.chars().count() as u16).sum();

    let [left_area, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(right_width),
    ]).areas(area);

    let left_para = Paragraph::new(Line::from(spans)).style(base);
    frame.render_widget(left_para, left_area);

    let right_para = Paragraph::new(Line::from(right_spans)).style(base);
    frame.render_widget(right_para, right_area);
}
