//! ASCII QR code renderer using Unicode block characters.
//!
//! Uses the `qrcode` crate to generate the QR matrix, then renders each
//! module as a Unicode half-block character for compact display.

use qrcode::{QrCode, EcLevel};

/// Render a QR code for `data` as a multi-line UTF-8 string.
///
/// Uses `▀` (upper-half block) and `▄` (lower-half block) so two QR rows
/// fit in one terminal line, halving the vertical space.
pub fn render_qr_text(data: &str) -> String {
    match QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M) {
        Err(_) => format!("[QR error for: {}]", data),
        Ok(code) => {
            let width = code.width();
            let modules: Vec<bool> = code
                .into_colors()
                .iter()
                .map(|c| *c == qrcode::Color::Dark)
                .collect();

            let mut out = String::new();

            // Process rows in pairs (upper + lower)
            let mut row = 0;
            while row < width {
                let upper_row = row;
                let lower_row = row + 1;

                for col in 0..width {
                    let upper = modules.get(upper_row * width + col).copied().unwrap_or(false);
                    let lower = modules.get(lower_row * width + col).copied().unwrap_or(false);

                    let ch = match (upper, lower) {
                        (true, true)   => '█',
                        (true, false)  => '▀',
                        (false, true)  => '▄',
                        (false, false) => ' ',
                    };
                    out.push(ch);
                }
                out.push('\n');
                row += 2;
            }

            out
        }
    }
}
