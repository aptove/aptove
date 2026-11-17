use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

use crate::tui::widgets::qr_popup::{centered_rect, copy_btn_at, parse_pairing_fields, render_qr_popup};
use crate::tui::screens::wizard::render_form_dynamic;

/// Log level options shown in the picker (name, u8 value).
pub const LOG_LEVELS: &[(&str, u8)] = &[
    ("ERROR", 1),
    ("WARN",  2),
    ("INFO",  3),
    ("DEBUG", 4),
    ("TRACE", 5),
];

/// State for the multi-step push-configuration popup.
#[derive(Debug, Clone, PartialEq)]
pub enum PushPopupStep {
    /// Top-level option picker.
    /// `selected` = highlighted row; `active` = currently configured mode (0/1/2).
    Menu { selected: usize, active: usize },
    /// Aptove form: fields = [client_id, client_secret].
    AptoveForm { fields: [String; 2], field_idx: usize, error: Option<String> },
    /// Self-managed form: fields = [token_url, push_url, client_id, client_secret].
    SelfManagedForm { fields: [String; 4], field_idx: usize, error: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum PopupKind {
    /// `text_mode`: when true, show the raw pairing URL instead of the QR graphic.
    QrCode { text_mode: bool },
    /// Interactive log-level picker; `selected` is the highlighted row.
    LogLevel { selected: usize },
    /// Push notifications configuration (multi-step).
    PushConfig { step: PushPopupStep },
    /// Transport failed — shown instead of QR when the bridge is not running.
    TransportError,
    /// Confirmation prompt before rotating the auth token.
    RotateToken,
    /// Shown when Ctrl+U is pressed but no update is available.
    UpdateNotAvailable,
    /// Shown when Ctrl+U is pressed and an update exists; Enter confirms download.
    UpdateConfirm { version: String },
}


pub fn render_popup(
    frame: &mut Frame,
    kind: &PopupKind,
    qr_string: &Option<String>,
    pairing_url: &Option<String>,
    pairing_secs: Option<u64>,
    transport: &str,
) {
    match kind {
        PopupKind::QrCode { text_mode } => {
            let qr = qr_string.as_deref().unwrap_or("No QR code available yet.\nStart Aptove first.");
            let url = pairing_url.as_deref().unwrap_or("No pairing URL available yet.");
            render_qr_popup(frame, frame.area(), pairing_secs, qr, *text_mode, url, transport);
        }
        PopupKind::LogLevel { selected } => {
            render_log_level_popup(frame, *selected);
        }
        PopupKind::PushConfig { step } => {
            render_push_popup(frame, step);
        }
        PopupKind::TransportError => {
            render_transport_error_popup(frame);
        }
        PopupKind::RotateToken => {
            render_rotate_token_popup(frame);
        }
        PopupKind::UpdateNotAvailable => {
            render_update_not_available_popup(frame);
        }
        PopupKind::UpdateConfirm { version } => {
            render_update_confirm_popup(frame, version);
        }
    }
}

// ── Push configuration ────────────────────────────────────────────────────────

const PUSH_REGISTER_URL: &str = "https://dash.aptove.com";
pub(crate) const PUSH_SERVICE_DOCS_URL: &str = "https://docs.aptove.com/push-service/overview";
pub(crate) const PUSH_SERVER_DOCS_URL: &str  = "https://docs.aptove.com/push-server/overview";
const PUSH_MENU_LABELS: &[&str] = &[
    "No Push",
    "Aptove Push Service  (recommended)",
    "Self Managed Push Server",
];

fn render_push_popup(frame: &mut Frame, step: &PushPopupStep) {
    match step {
        PushPopupStep::Menu { selected, active } => render_push_menu(frame, *selected, *active),
        PushPopupStep::AptoveForm { fields, field_idx, error } => {
            render_aptove_form(frame, fields, *field_idx, error.as_deref());
        }
        PushPopupStep::SelfManagedForm { fields, field_idx, error } => {
            render_self_managed_form(frame, fields, *field_idx, error.as_deref());
        }
    }
}

fn render_push_menu(frame: &mut Frame, selected: usize, active: usize) {
    let area = centered_rect(66, 55, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Push Notifications ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Menu items.
    let items: Vec<ListItem> = PUSH_MENU_LABELS.iter().enumerate().map(|(i, &label)| {
        let prefix = if i == selected { "> " } else { "  " };
        let suffix = if i == active { "  [active]" } else { "" };
        let style = if i == selected {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(format!("{}{}{}", prefix, label, suffix)).style(style)
    }).collect();

    let list_area = Rect { y: inner.y + 1, height: 3, ..inner };
    frame.render_widget(List::new(items), list_area);

    // Context-sensitive content below the list (matches wizard behaviour).
    match selected {
        0 => {
            frame.render_widget(
                Paragraph::new("Without push, messages buffer locally and deliver when the app reconnects.")
                    .style(Style::default().fg(Color::DarkGray)),
                Rect { y: inner.y + 5, height: 1, ..inner },
            );
        }
        1 => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Learn more: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(PUSH_SERVICE_DOCS_URL, Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)),
                ])),
                Rect { y: inner.y + 5, height: 1, ..inner },
            );
        }
        _ => {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Learn more: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(PUSH_SERVER_DOCS_URL, Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)),
                ])),
                Rect { y: inner.y + 5, height: 1, ..inner },
            );
        }
    }

    // Hint.
    let hint_y = inner.y + inner.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new("↑/↓ navigate   Enter select   Esc cancel")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: hint_y, height: 1, ..inner },
    );
}

fn render_aptove_form(frame: &mut Frame, fields: &[String; 2], field_idx: usize, error: Option<&str>) {
    let area = centered_rect(68, 62, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Aptove Push Service ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Registration link (clickable).
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Register at: ", Style::default().fg(Color::DarkGray)),
            Span::styled(PUSH_REGISTER_URL, Style::default().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)),
        ])),
        Rect { y: inner.y + 1, height: 1, ..inner },
    );

    // Fields (leave 1 row at the bottom for the hint).
    let labels = ["Client ID:", "Client Secret:"];
    let form_area = Rect { y: inner.y + 3, height: inner.height.saturating_sub(4), ..inner };
    render_form_dynamic(frame, form_area, &labels, fields.as_slice(), field_idx, error);

    // Hint.
    frame.render_widget(
        Paragraph::new("\u{2191}/\u{2193} navigate   Enter confirm   Esc back")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}

fn render_self_managed_form(frame: &mut Frame, fields: &[String; 4], field_idx: usize, error: Option<&str>) {
    let area = centered_rect(72, 72, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Self Managed Push Server ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Fields (token URL first, matching wizard order; leave 1 row at bottom for hint).
    let labels = ["Token Service URL:", "Push Service URL:", "Client ID:", "Client Secret:"];
    let form_area = Rect { y: inner.y + 1, height: inner.height.saturating_sub(2), ..inner };
    render_form_dynamic(frame, form_area, &labels, fields.as_slice(), field_idx, error);

    // Hint.
    frame.render_widget(
        Paragraph::new("\u{2191}/\u{2193} navigate   Enter confirm   Esc back")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}

// ── Copy-button hit-testing (QrCode text mode) ────────────────────────────────

/// Returns `(field_label, field_value)` if `(col, row)` lands on a numbered copy
/// button inside the QrCode text-mode popup.  Returns `None` otherwise.
pub fn copy_field_at(
    kind: &PopupKind,
    col: u16,
    row: u16,
    term: Rect,
    pairing_url: &Option<String>,
) -> Option<(&'static str, String)> {
    if !matches!(kind, PopupKind::QrCode { text_mode: true }) {
        return None;
    }
    let url = pairing_url.as_deref()?;

    let border_inner = |area: Rect| Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let inner = border_inner(centered_rect(70, 80, term));

    if let Some((idx, value)) = copy_btn_at(col, row, inner, url) {
        let fields = parse_pairing_fields(url);
        let label = fields.get(idx).map(|f| f.label)?;
        return Some((label, value));
    }
    None
}

// ── URL hit-testing ───────────────────────────────────────────────────────────

/// Return the static URL string if `(col, row)` lands on a clickable URL inside
/// the current popup, given the terminal's full area.  Returns `None` otherwise.
pub fn url_at(kind: &PopupKind, col: u16, row: u16, term: Rect) -> Option<&'static str> {
    let on_row = |inner: Rect, y: u16| row == y && col >= inner.x && col < inner.x + inner.width;
    let border_inner = |area: Rect| -> Rect {
        // Block with all borders shrinks each side by 1.
        Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        }
    };

    if let PopupKind::PushConfig { step } = kind {
        match step {
            PushPopupStep::Menu { selected, .. } => {
                // Context-sensitive link at y+5 (items 1 and 2 only; item 0 shows a note).
                let inner = border_inner(centered_rect(66, 55, term));
                if on_row(inner, inner.y + 5) {
                    return match selected {
                        1 => Some(PUSH_SERVICE_DOCS_URL),
                        2 => Some(PUSH_SERVER_DOCS_URL),
                        _ => None,
                    };
                }
            }
            PushPopupStep::AptoveForm { .. } => {
                let inner = border_inner(centered_rect(68, 62, term));
                if on_row(inner, inner.y + 1) {
                    return Some(PUSH_REGISTER_URL);
                }
            }
            PushPopupStep::SelfManagedForm { .. } => {}
        }
    }
    None
}

// ── Transport error ───────────────────────────────────────────────────────────

fn render_transport_error_popup(frame: &mut Frame) {
    use ratatui::widgets::Wrap;
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" QR Code Unavailable ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(
            "The transport is not running.\n\n\
             Aptove is retrying automatically.\n\
             Use /transport to change transports."
        )
        .style(Style::default().fg(Color::Yellow))
        .wrap(Wrap { trim: false }),
        Rect { x: inner.x + 2, y: inner.y + 2, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(3) },
    );

    frame.render_widget(
        Paragraph::new("Esc to close")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}

// ── Rotate token confirmation ─────────────────────────────────────────────────

fn render_rotate_token_popup(frame: &mut Frame) {
    use ratatui::widgets::Wrap;
    let area = centered_rect(56, 36, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Rotate Auth Token ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(
            "Generate a new auth token?\n\n\
             All paired devices will be disconnected\n\
             and must re-scan the QR code to reconnect.\n\n\
             All other settings are preserved."
        )
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false }),
        Rect { x: inner.x + 2, y: inner.y + 1, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(3) },
    );

    frame.render_widget(
        Paragraph::new("⏎  Rotate  ·  Esc  Cancel")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}

// ── Log level picker ──────────────────────────────────────────────────────────

fn render_log_level_popup(frame: &mut Frame, selected: usize) {
    let area = centered_rect(40, 50, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Log Level (↑/↓ select   Enter confirm   Esc cancel) ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let items: Vec<ListItem> = LOG_LEVELS.iter().enumerate().map(|(i, (name, _))| {
        let label = format!("  {}  ", name);
        let style = if i == selected {
            Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(label).style(style)
    }).collect();

    // Centre the list vertically inside the block.
    let list_area = Rect {
        y: inner.y + inner.height.saturating_sub(LOG_LEVELS.len() as u16) / 2,
        height: LOG_LEVELS.len() as u16,
        ..inner
    };
    frame.render_widget(List::new(items), list_area);
}

// ── Update popups ─────────────────────────────────────────────────────────────

fn render_update_not_available_popup(frame: &mut Frame) {
    use ratatui::widgets::Wrap;
    let area = centered_rect(56, 36, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Software Update ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new("Aptove is up to date.\nNo new update available.")
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false }),
        Rect { x: inner.x + 2, y: inner.y + 2, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(3) },
    );

    frame.render_widget(
        Paragraph::new("⏎  Dismiss")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}

fn render_update_confirm_popup(frame: &mut Frame, version: &str) {
    use ratatui::widgets::Wrap;
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Update Available ")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::Yellow));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    frame.render_widget(
        Paragraph::new(format!(
            "v{} is ready to install.\n\n\
             The binary will be replaced in-place.\n\
             Your .aptove/ config is preserved.",
            version
        ))
        .style(Style::default().fg(Color::White))
        .wrap(Wrap { trim: false }),
        Rect { x: inner.x + 2, y: inner.y + 1, width: inner.width.saturating_sub(4), height: inner.height.saturating_sub(3) },
    );

    frame.render_widget(
        Paragraph::new("⏎  Install  ·  Esc  Cancel")
            .style(Style::default().fg(Color::DarkGray)),
        Rect { y: inner.y + inner.height.saturating_sub(1), height: 1, ..inner },
    );
}
