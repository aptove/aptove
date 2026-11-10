use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// Returns a centered `Rect` of the given percentage of the outer `Rect`.
pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// A single labelled field parsed out of a pairing URL for text-mode display.
pub struct PairingField {
    pub label: &'static str,
    pub value: String,
    /// Whether this field gets a numbered copy button.
    /// Informational-only fields (Connection Type, Use HTTPS) do not.
    pub copyable: bool,
}

/// Parse a pairing URL into individual fields matching the iOS manual-entry form.
///
/// Handles `/pair/local`, `/pair/tailscale`, and `/pair/cloudflare` paths.
pub fn parse_pairing_fields(url: &str) -> Vec<PairingField> {
    let mut fields = Vec::new();

    let path_type = if url.contains("/pair/local") {
        "local"
    } else if url.contains("/pair/tailscale") {
        "tailscale"
    } else if url.contains("/pair/cloudflare") {
        "cloudflare"
    } else {
        return fields;
    };

    // Extract host (everything between scheme:// and the first /)
    let after_scheme = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = after_scheme.split('/').next().unwrap_or("").to_string();

    // Parse query string
    let query = url.split('?').nth(1).unwrap_or("");
    let mut code = String::new();
    let mut fp = String::new();
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            code = v.to_string();
        } else if let Some(v) = pair.strip_prefix("fp=") {
            fp = urlencoding::decode(v).unwrap_or_default().into_owned();
        }
    }

    let is_https = url.starts_with("https://");

    match path_type {
        "local" => {
            fields.push(PairingField { label: "Connection Type",         value: "Local".to_string(),                                 copyable: false });
            fields.push(PairingField { label: "Aptove Address",          value: host,                                                copyable: true  });
            fields.push(PairingField { label: "Use HTTPS",               value: if is_https { "Yes" } else { "No" }.to_string(),     copyable: false });
            fields.push(PairingField { label: "Pairing Code",            value: code,                                                copyable: true  });
            if !fp.is_empty() {
                fields.push(PairingField { label: "Certificate Fingerprint", value: fp,                                              copyable: true  });
            }
        }
        "tailscale" => {
            fields.push(PairingField { label: "Connection Type",         value: "Tailscale".to_string(),                             copyable: false });
            fields.push(PairingField { label: "Aptove Address",          value: host,                                                copyable: true  });
            fields.push(PairingField { label: "Pairing Code",            value: code,                                                copyable: true  });
            if !fp.is_empty() {
                fields.push(PairingField { label: "Certificate Fingerprint", value: fp,                                              copyable: true  });
            }
        }
        "cloudflare" => {
            fields.push(PairingField { label: "Connection Type",         value: "Cloudflare".to_string(),                            copyable: false });
            fields.push(PairingField { label: "Aptove URL",              value: format!("https://{}", host),                         copyable: true  });
            fields.push(PairingField { label: "Pairing Code",            value: code,                                                copyable: true  });
        }
        _ => {}
    }

    fields
}

/// Render a QR code popup (or its text-mode equivalent) in the center of the screen.
///
/// `pairing_secs`: remaining seconds shown in the title; `Some(0)` shows
/// "renewing…", `None` omits the countdown entirely.
/// `text_mode`: when true, shows the parsed pairing fields for manual entry.
/// `transport`: the active transport name shown in the title (e.g. "local", "tailscale-serve").
pub fn render_qr_popup(
    frame: &mut Frame,
    area: Rect,
    pairing_secs: Option<u64>,
    qr_string: &str,
    text_mode: bool,
    url: &str,
    transport: &str,
) {
    let popup_area = centered_rect(70, 80, area);

    frame.render_widget(Clear, popup_area);

    let toggle_hint = if text_mode { "[Q] show QR" } else { "[T] show text" };
    let transport_badge = if transport.is_empty() {
        String::new()
    } else {
        format!(" [{}]", transport)
    };
    let title: Line = match pairing_secs {
        Some(0) => Line::from(vec![
            Span::raw(format!(" Pairing QR Code{}", transport_badge)),
            Span::styled(" — renewing…", Style::default().fg(Color::Red)),
            Span::raw(format!("  {}  Esc close ", toggle_hint)),
        ]),
        Some(s) => Line::from(vec![
            Span::raw(format!(" Pairing QR Code{}", transport_badge)),
            Span::styled(
                format!(" — expires in {}s", s),
                Style::default().fg(Color::Red),
            ),
            Span::raw(format!("  {}  Esc close ", toggle_hint)),
        ]),
        None => Line::from(format!(" Pairing QR Code{}  {}  Esc close ", transport_badge, toggle_hint)),
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if text_mode {
        render_text_fields(frame, inner, url);
    } else {
        let text = Text::raw(qr_string);
        let para = Paragraph::new(text)
            .style(Style::default().bg(Color::Black).fg(Color::White));
        frame.render_widget(para, inner);
    }
}

/// Label column width (characters).
const LABEL_WIDTH: u16 = 24;
/// Width of the `[N]` copy button.
const BTN_WIDTH: u16 = 3;

/// Split `s` into consecutive slices of at most `width` characters.
/// Used to display long values (e.g. certificate fingerprints) without clipping.
fn char_chunks(s: &str, width: u16) -> Vec<String> {
    if width == 0 { return vec![s.to_string()]; }
    let w = width as usize;
    let chars: Vec<char> = s.chars().collect();
    chars.chunks(w).map(|c| c.iter().collect()).collect()
}

/// Per-field layout descriptor computed by [`field_layout`].
struct FieldPos {
    /// Absolute y of the label row (where `[N]` also lives).
    label_y: u16,
    /// Number of extra rows used for the wrapped value (0 = value is inline).
    extra_rows: u16,
}

/// Compute the row positions of all fields given the padded content rect.
///
/// Short fields (value fits beside the label) occupy 1 row.
/// Long fields (value wider than the inline space) put the label (+ `[N]` if
/// copyable) on their first row and the value — character-wrapped — on the
/// following rows.
fn field_layout(fields: &[PairingField], padded: Rect) -> Vec<FieldPos> {
    // Copyable fields reserve space for the [N] button; non-copyable ones use full width.
    let single_val_w_btn    = padded.width.saturating_sub(LABEL_WIDTH + BTN_WIDTH + 1);
    let single_val_w_no_btn = padded.width.saturating_sub(LABEL_WIDTH);
    // Value indented by 2 when it spills below the label row.
    let two_row_val_w = padded.width.saturating_sub(2);

    let mut result = Vec::with_capacity(fields.len());
    let mut y = padded.y + 2; // first field row = after header + blank line

    for field in fields {
        let single_val_w = if field.copyable { single_val_w_btn } else { single_val_w_no_btn };
        let vlen = field.value.chars().count() as u16;
        if vlen <= single_val_w {
            result.push(FieldPos { label_y: y, extra_rows: 0 });
            y += 1;
        } else {
            let chunks = if two_row_val_w > 0 {
                (vlen + two_row_val_w - 1) / two_row_val_w
            } else { 1 };
            result.push(FieldPos { label_y: y, extra_rows: chunks });
            y += 1 + chunks;
        }
    }
    result
}

/// Render parsed pairing fields with numbered copy buttons inside `inner`.
fn render_text_fields(frame: &mut Frame, inner: Rect, url: &str) {
    let padded = text_padded_rect(inner);
    let fields = parse_pairing_fields(url);
    let layout = field_layout(&fields, padded);

    let btn_x = padded.x + padded.width.saturating_sub(BTN_WIDTH);
    let single_val_w_btn    = padded.width.saturating_sub(LABEL_WIDTH + BTN_WIDTH + 1);
    let single_val_w_no_btn = padded.width.saturating_sub(LABEL_WIDTH);
    let two_row_val_w = padded.width.saturating_sub(2);
    let bottom = padded.y + padded.height;

    // Header
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Enter these fields manually in the Aptove iOS app:",
            Style::default().fg(Color::DarkGray),
        )),
        Rect { y: padded.y, height: 1, ..padded },
    );

    let mut btn_idx = 1usize; // counts only copyable fields

    for (field, pos) in fields.iter().zip(layout.iter()) {
        if pos.label_y + 2 >= bottom { break; }

        // Label
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("{:<width$}", field.label, width = LABEL_WIDTH as usize),
                Style::default().fg(Color::DarkGray),
            )),
            Rect { x: padded.x, y: pos.label_y, width: LABEL_WIDTH, height: 1 },
        );

        // [N] copy button — only for copyable fields
        if field.copyable {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("[{}]", btn_idx),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Rect { x: btn_x, y: pos.label_y, width: BTN_WIDTH, height: 1 },
            );
            btn_idx += 1;
        }

        let single_val_w = if field.copyable { single_val_w_btn } else { single_val_w_no_btn };

        if pos.extra_rows == 0 {
            // Value inline with label
            frame.render_widget(
                Paragraph::new(Span::styled(
                    field.value.clone(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )),
                Rect {
                    x: padded.x + LABEL_WIDTH,
                    y: pos.label_y,
                    width: single_val_w,
                    height: 1,
                },
            );
        } else {
            // Value on the next rows, indented, character-wrapped so nothing is clipped
            let value_y = pos.label_y + 1;
            if value_y < bottom {
                let chunks = char_chunks(&field.value, two_row_val_w);
                let lines: Vec<Line> = chunks.into_iter()
                    .map(|c| Line::from(Span::styled(
                        c,
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    )))
                    .collect();
                let avail_h = (bottom.saturating_sub(value_y + 1)).min(pos.extra_rows);
                if avail_h > 0 {
                    frame.render_widget(
                        Paragraph::new(Text::from(lines)),
                        Rect {
                            x: padded.x + 2,
                            y: value_y,
                            width: two_row_val_w,
                            height: avail_h,
                        },
                    );
                }
            }
        }
    }

    // Bottom hint
    let hint_y = padded.y + padded.height.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Span::styled(
            "Click [N] or press number key to copy a field to clipboard",
            Style::default().fg(Color::DarkGray),
        )),
        Rect { y: hint_y, height: 1, ..padded },
    );
}

/// Inner rect with padding applied (matches rendering padding for hit-testing).
pub fn text_padded_rect(inner: Rect) -> Rect {
    Rect {
        x: inner.x + 2,
        y: inner.y + 1,
        width: inner.width.saturating_sub(4),
        height: inner.height.saturating_sub(2),
    }
}

/// Returns `(field_index, value)` if `(col, row)` lands on a `[N]` copy button
/// inside the text-mode field list. `inner` must be the block's inner rect.
pub fn copy_btn_at(col: u16, row: u16, inner: Rect, url: &str) -> Option<(usize, String)> {
    let padded = text_padded_rect(inner);
    let btn_x = padded.x + padded.width.saturating_sub(BTN_WIDTH);

    if col < btn_x || col >= btn_x + BTN_WIDTH {
        return None;
    }

    let fields = parse_pairing_fields(url);
    let layout = field_layout(&fields, padded);

    for (i, (field, pos)) in fields.iter().zip(layout.iter()).enumerate() {
        if pos.label_y + 2 >= padded.y + padded.height { break; }
        if !field.copyable { continue; }
        if row == pos.label_y {
            return Some((i, field.value.clone()));
        }
    }
    None
}

/// Render a generic text popup in the center of the screen.
pub fn render_text_popup(frame: &mut Frame, area: Rect, title: &str, content: &str) {
    let popup_area = centered_rect(60, 60, area);
    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(format!(" {} ", title))
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black).fg(Color::White));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let para = Paragraph::new(content)
        .style(Style::default().bg(Color::Black).fg(Color::White));
    frame.render_widget(para, inner);
}
