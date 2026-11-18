/// Size-rotating file log layer for tracing.
///
/// Writes all events at DEBUG level and above to
/// `<log_dir>/aptove.log`. When the file reaches `max_bytes`, it is
/// rotated: `aptove.log.{count-1}` is deleted, each `.N` is renamed to
/// `.N+1`, `aptove.log` becomes `aptove.log.1`, and a fresh
/// `aptove.log` is opened. At most `file_count` files are kept on disk.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

/// When `false` (the default), message content is stripped from log lines —
/// only byte counts and method names are recorded.  Set to `true` via
/// `log_message_content = true` in `common.toml` to include raw content for
/// debugging.  **Never enable in production: prompts and agent replies will
/// appear in log files readable by any process that can open them.**
static LOG_MESSAGE_CONTENT: AtomicBool = AtomicBool::new(false);

/// Set whether message content should be included in log output.
/// Call once at startup after loading `CommonConfig`.
pub fn set_log_message_content(enabled: bool) {
    LOG_MESSAGE_CONTENT.store(enabled, Ordering::Relaxed);
}

/// Returns `": <first max_chars chars of text>"` when content logging is
/// enabled, or an empty string otherwise.  Use this to append message content
/// to a log line without hard-coding the content into the format string.
pub fn content_suffix(text: &str, max_chars: usize) -> String {
    if LOG_MESSAGE_CONTENT.load(Ordering::Relaxed) {
        format!(": {}", text.chars().take(max_chars).collect::<String>())
    } else {
        String::new()
    }
}

/// Returns the first 8 characters of `token` followed by `"..."`.
/// Use when logging device tokens or other opaque credentials so the log entry
/// is useful for correlation without leaking the full value.
pub fn redact_token(token: &str) -> String {
    let prefix: String = token.chars().take(8).collect();
    format!("{}...", prefix)
}

pub struct FileLogLayer {
    log_dir: PathBuf,
    max_bytes: u64,
    file_count: usize,
    state: Mutex<FileState>,
}

struct FileState {
    writer: Option<BufWriter<File>>,
    /// Bytes written to the current `aptove.log` since it was opened.
    written: u64,
}

impl FileLogLayer {
    /// Create a new layer. `log_dir` is created if it does not exist.
    ///
    /// - `max_mb`: max size per file in megabytes.
    /// - `file_count`: number of files to keep (including the active one).
    pub fn new(log_dir: PathBuf, max_mb: u64, file_count: usize) -> Self {
        let _ = fs::create_dir_all(&log_dir);
        // Restrict the log directory to owner-only so rotated files cannot be
        // read by other local users even before we set per-file permissions.
        let _ = crate::file_permissions::restrict_dir_to_owner(&log_dir);

        let log_path = log_dir.join("aptove.log");
        let (writer, written) = open_append(&log_path);

        Self {
            log_dir,
            max_bytes: max_mb.saturating_mul(1024 * 1024),
            file_count: file_count.max(1),
            state: Mutex::new(FileState { writer, written }),
        }
    }

    /// Rotate: delete oldest backup, shift each .N → .N+1, rename
    /// current → .1, then the caller should open a fresh aptove.log.
    fn rotate(&self) {
        // Delete the oldest backup if we are at capacity.
        let oldest = self.log_dir.join(format!("aptove.log.{}", self.file_count - 1));
        if oldest.exists() {
            let _ = fs::remove_file(&oldest);
        }
        // Shift existing backups: .N → .(N+1), starting from the highest.
        for n in (1..self.file_count - 1).rev() {
            let from = self.log_dir.join(format!("aptove.log.{}", n));
            let to   = self.log_dir.join(format!("aptove.log.{}", n + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        // Rename current log to .1.
        let current = self.log_dir.join("aptove.log");
        if current.exists() {
            let _ = fs::rename(&current, self.log_dir.join("aptove.log.1"));
        }
    }
}

fn open_append(path: &PathBuf) -> (Option<BufWriter<File>>, u64) {
    let existing = path.metadata().map(|m| m.len()).unwrap_or(0);
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    match result {
        Ok(file) => {
            // Restrict to owner-read/write immediately after opening so the
            // window where other users can read a newly created file is as
            // small as possible.
            let _ = crate::file_permissions::restrict_file_to_owner(path);
            (Some(BufWriter::new(file)), existing)
        }
        Err(_) => (None, existing),
    }
}

impl<S: Subscriber> Layer<S> for FileLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Write DEBUG and above; skip TRACE to keep file sizes reasonable.
        let level = *event.metadata().level();
        if level == Level::TRACE {
            return;
        }

        let level_str = match level {
            Level::ERROR => "ERROR",
            Level::WARN  => "WARN ",
            Level::INFO  => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };

        let mut visitor = MsgVisitor::default();
        event.record(&mut visitor);

        let now = chrono::Local::now();
        let line = format!(
            "{} {} {}\n",
            now.format("%Y-%m-%d %H:%M:%S"),
            level_str,
            visitor.message,
        );
        let line_len = line.len() as u64;

        let Ok(mut st) = self.state.lock() else { return };

        // Rotate if this write would exceed the per-file limit.
        if st.written + line_len > self.max_bytes {
            st.writer = None; // close current file before renaming
            self.rotate();
            let log_path = self.log_dir.join("aptove.log");
            let (w, _) = open_append(&log_path);
            st.writer = w;
            st.written = 0;
        }

        // Lazily (re)open on first use or after a rotation.
        if st.writer.is_none() {
            let log_path = self.log_dir.join("aptove.log");
            let (w, existing) = open_append(&log_path);
            st.writer = w;
            st.written = existing;
        }

        if let Some(ref mut w) = st.writer {
            if w.write_all(line.as_bytes()).is_ok() {
                let _ = w.flush();
                st.written += line_len;
            }
        }
    }
}

#[derive(Default)]
struct MsgVisitor {
    message: String,
}

impl tracing::field::Visit for MsgVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[cfg(unix)]
    fn new_creates_dir_with_0700_and_file_with_0600() {
        use std::os::unix::fs::PermissionsExt;

        let base = tempdir().unwrap();
        let log_dir = base.path().join("logs");

        let layer = FileLogLayer::new(log_dir.clone(), 1, 2);
        // Trigger a write so the log file is created.
        drop(layer);

        let dir_mode = fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "log directory must be owner-only (0700)");

        let log_file = log_dir.join("aptove.log");
        if log_file.exists() {
            let file_mode = fs::metadata(&log_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600, "log file must be owner-only (0600)");
        }
    }

    #[test]
    fn content_suffix_hidden_by_default() {
        set_log_message_content(false);
        assert_eq!(content_suffix("hello world", 200), "");
    }

    #[test]
    fn content_suffix_visible_when_enabled() {
        set_log_message_content(true);
        assert_eq!(content_suffix("hello world", 200), ": hello world");
        // Reset so other tests are unaffected.
        set_log_message_content(false);
    }

    #[test]
    fn redact_token_shows_prefix_only() {
        assert_eq!(redact_token("abcdefgh12345678"), "abcdefgh...");
        assert_eq!(redact_token("short"), "short...");
        assert_eq!(redact_token(""), "...");
    }
}
