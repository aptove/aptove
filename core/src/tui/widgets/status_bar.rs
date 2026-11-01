use ratatui::{
    layout::Rect,
    style::{Color, Style},
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
) {
    let base = Style::default().bg(Color::DarkGray).fg(Color::White);
    let mut spans: Vec<Span> = vec![Span::styled(
        format!(" Aptove v{}", version),
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

    let para = Paragraph::new(Line::from(spans)).style(base);
    frame.render_widget(para, area);
}
