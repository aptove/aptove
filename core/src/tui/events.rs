use crossterm::event::{KeyEvent, MouseEvent};
use crate::common_config::TransportConfig;

/// A single log record captured from the tracing subscriber.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

/// Events emitted by bridge internals → TUI.
#[derive(Debug, Clone)]
pub enum BridgeEvent {
    TransportUp { name: String, addr: String },
    TransportDown { name: String },
    ClientConnected { session_id: String },
    ClientDisconnected { session_id: String },
    PairingCompleted,
    PairingUrlReady { url: String, transport: String },
    PortConflictResolved { old_port: u16, new_port: u16 },
    AgentSpawned { command: String },
    AgentExited,
    TlsFingerprint { fingerprint: String },
    PushRegistered,
    /// A push notification was successfully delivered to at least one device.
    PushNotified,
    /// A push notification delivery attempt failed.
    PushFailed,
    BridgeStopped,
    BridgeError { message: String },
}

/// Top-level event type that drives the TUI draw loop.
#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Bridge(BridgeEvent),
    Log(LogRecord),
    Tick,
    Resize(u16, u16),
    /// Result of an async plugin setup triggered from the wizard.
    PluginSetupResult { transport_name: String, result: Result<TransportConfig, String> },
    /// Result of an async test-push triggered from the running screen.
    TestPushResult(Result<bool, String>),
    /// Fired after a delay to trigger an automatic transport retry.
    RetryBridge,
    /// Background version check completed. Some("1.4.1") = update available.
    /// None = already up-to-date or network not reachable.
    UpdateCheckResult(Option<String>),
    /// A progress message from an in-progress binary download/replace.
    UpdateProgress(String),
    /// Self-update completed. Ok = new version string; Err = error message.
    UpdateComplete(Result<String, String>),
}

/// Commands sent from the TUI to the bridge runner.
#[derive(Debug, Clone)]
pub enum TuiCommand {
    Quit,
    Reconnect,
    RefreshQr,
}
