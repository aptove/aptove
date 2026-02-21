//! ACP Stdio Transport
//!
//! JSON-RPC 2.0 transport over stdin/stdout. Reads newline-delimited JSON
//! from stdin, dispatches to registered handlers, and writes responses to
//! stdout. All logging goes to stderr via `tracing`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// JSON-RPC wire types
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

/// A raw JSON-RPC message as received from stdin.
#[derive(Debug, Deserialize)]
pub struct RawJsonRpcMessage {
    pub jsonrpc: Option<String>,
    pub id: Option<JsonRpcId>,
    pub method: Option<String>,
    pub params: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcErrorObject>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Outgoing JSON-RPC response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// Outgoing JSON-RPC notification (no id).
#[derive(Debug, Serialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// Anything we can write to stdout.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum OutgoingMessage {
    Response(JsonRpcResponse),
    Notification(JsonRpcNotification),
}

/// Parsed incoming message ready for dispatch.
#[derive(Debug)]
pub enum IncomingMessage {
    /// A request expecting a response.
    Request {
        id: JsonRpcId,
        method: String,
        params: Option<Value>,
    },
    /// A one-way notification (no response expected).
    Notification {
        method: String,
        params: Option<Value>,
    },
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A sender handle that can write JSON-RPC messages to stdout.
///
/// Cheaply cloneable — hand one to every subsystem that needs to send
/// notifications (session updates, tool call progress, etc.).
#[derive(Clone)]
pub struct TransportSender {
    tx: mpsc::Sender<OutgoingMessage>,
}

impl TransportSender {
    /// Send a successful JSON-RPC response.
    pub async fn send_response(&self, id: JsonRpcId, result: Value) -> Result<()> {
        debug!("queueing JSON-RPC response for id: {:?}", id);
        let msg = OutgoingMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: id.clone(),
            result: Some(result),
            error: None,
        });
        self.tx.send(msg).await.context("stdout writer closed")?;
        debug!("response queued successfully for id: {:?}", id);
        Ok(())
    }

    /// Send a JSON-RPC error response.
    pub async fn send_error(
        &self,
        id: JsonRpcId,
        code: i32,
        message: impl Into<String>,
    ) -> Result<()> {
        let msg = OutgoingMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcErrorObject {
                code,
                message: message.into(),
                data: None,
            }),
        });
        self.tx.send(msg).await.context("stdout writer closed")?;
        Ok(())
    }

    /// Send a JSON-RPC notification to the client (no id, no response expected).
    pub async fn send_notification(&self, method: &str, params: Value) -> Result<()> {
        let msg = OutgoingMessage::Notification(JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: method.to_string(),
            params: Some(params),
        });
        self.tx.send(msg).await.context("stdout writer closed")?;
        Ok(())
    }
}

/// ACP Stdio Transport.
///
/// Reads newline-delimited JSON-RPC from stdin, provides a channel of
/// parsed [`IncomingMessage`]s, and a [`TransportSender`] for writing
/// responses and notifications to stdout.
pub struct StdioTransport {
    /// Channel receiving parsed incoming messages.
    incoming_rx: mpsc::Receiver<IncomingMessage>,
    /// Sender handle for writing to stdout (cloneable).
    sender: TransportSender,
}

impl StdioTransport {
    /// Create a new transport. Spawns background tasks for stdin reading
    /// and stdout writing.
    pub fn new() -> Self {
        // Outgoing messages → stdout writer
        let (out_tx, out_rx) = mpsc::channel::<OutgoingMessage>(256);
        tokio::spawn(Self::stdout_writer_task(out_rx));

        // Stdin reader → incoming messages
        let (in_tx, in_rx) = mpsc::channel::<IncomingMessage>(256);
        tokio::spawn(Self::stdin_reader_task(in_tx));

        Self {
            incoming_rx: in_rx,
            sender: TransportSender { tx: out_tx },
        }
    }

    /// Get a cloneable sender handle for writing to stdout.
    pub fn get_sender(&self) -> TransportSender {
        self.sender.clone()
    }

    /// Receive the next incoming message. Returns `None` when stdin closes.
    pub async fn recv(&mut self) -> Option<IncomingMessage> {
        self.incoming_rx.recv().await
    }

    // -- background tasks ---------------------------------------------------

    async fn stdin_reader_task(tx: mpsc::Sender<IncomingMessage>) {
        let stdin = io::stdin();
        let reader = BufReader::new(stdin);
        let mut lines = reader.lines();

        info!("Aptove transport: reading from stdin");

        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            debug!(raw = %line, "← recv");

            let raw: RawJsonRpcMessage = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    warn!(err = %e, "invalid JSON-RPC, skipping");
                    continue;
                }
            };

            let msg = match (raw.method, raw.id) {
                (Some(method), Some(id)) => IncomingMessage::Request {
                    id,
                    method,
                    params: raw.params,
                },
                (Some(method), None) => IncomingMessage::Notification {
                    method,
                    params: raw.params,
                },
                _ => {
                    debug!("ignoring message without method");
                    continue;
                }
            };

            if tx.send(msg).await.is_err() {
                break; // handler dropped
            }
        }

        info!("stdin closed");
    }

    async fn stdout_writer_task(mut rx: mpsc::Receiver<OutgoingMessage>) {
        let mut stdout = io::stdout();
        info!("stdout writer task started");

        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => {
                    error!(err = %e, "failed to serialize outgoing message");
                    continue;
                }
            };
            debug!(raw = %json, "→ send");

            let line = format!("{}\n", json);
            if let Err(e) = stdout.write_all(line.as_bytes()).await {
                error!(err = %e, "stdout write failed");
                break;
            }
            if let Err(e) = stdout.flush().await {
                error!(err = %e, "stdout flush failed");
                break;
            }
            debug!("message written and flushed to stdout");
        }
        info!("stdout writer task exiting");
    }
}

/// ACP In-Process Transport.
///
/// Identical protocol to [`StdioTransport`] but backed by `tokio::sync::mpsc`
/// channels instead of stdin/stdout. Used when the agent runs in the same
/// process as the bridge server (`aptove run`).
///
/// The bridge writes newline-delimited JSON-RPC into `input_tx`;
/// the agent reads from `input_rx`. Agent responses are written to
/// `output_tx`; the bridge reads them from `output_rx`.
pub struct InProcessTransport {
    incoming_rx: mpsc::Receiver<IncomingMessage>,
    sender: TransportSender,
}

impl InProcessTransport {
    /// Create a new in-process transport connected to the supplied channel pair.
    ///
    /// - `input_rx`: receives raw JSON-RPC bytes from the bridge (replaces stdin)
    /// - `output_tx`: sends raw JSON-RPC bytes back to the bridge (replaces stdout)
    pub fn new(
        input_rx: mpsc::Receiver<Vec<u8>>,
        output_tx: mpsc::Sender<Vec<u8>>,
    ) -> Self {
        let (out_tx, out_rx) = mpsc::channel::<OutgoingMessage>(256);
        tokio::spawn(Self::channel_writer_task(out_rx, output_tx));

        let (in_tx, in_rx) = mpsc::channel::<IncomingMessage>(256);
        tokio::spawn(Self::channel_reader_task(input_rx, in_tx));

        Self {
            incoming_rx: in_rx,
            sender: TransportSender { tx: out_tx },
        }
    }

    /// Get a cloneable sender handle.
    pub fn get_sender(&self) -> TransportSender {
        self.sender.clone()
    }

    /// Receive the next incoming message. Returns `None` when the input channel closes.
    pub async fn recv(&mut self) -> Option<IncomingMessage> {
        self.incoming_rx.recv().await
    }

    async fn channel_reader_task(
        mut rx: mpsc::Receiver<Vec<u8>>,
        tx: mpsc::Sender<IncomingMessage>,
    ) {
        while let Some(bytes) = rx.recv().await {
            let line = match std::str::from_utf8(&bytes) {
                Ok(s) => s.trim().to_string(),
                Err(e) => { warn!(err = %e, "invalid UTF-8 from bridge, skipping"); continue; }
            };
            if line.is_empty() { continue; }
            debug!(raw = %line, "← in-process recv");

            let raw: RawJsonRpcMessage = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => { warn!(err = %e, "invalid JSON-RPC, skipping"); continue; }
            };

            let msg = match (raw.method, raw.id) {
                (Some(method), Some(id)) => IncomingMessage::Request { id, method, params: raw.params },
                (Some(method), None) => IncomingMessage::Notification { method, params: raw.params },
                _ => { debug!("ignoring message without method"); continue; }
            };

            if tx.send(msg).await.is_err() { break; }
        }
    }

    async fn channel_writer_task(
        mut rx: mpsc::Receiver<OutgoingMessage>,
        tx: mpsc::Sender<Vec<u8>>,
    ) {
        while let Some(msg) = rx.recv().await {
            let json = match serde_json::to_string(&msg) {
                Ok(j) => j,
                Err(e) => { error!(err = %e, "failed to serialize outgoing message"); continue; }
            };
            debug!(raw = %json, "→ in-process send");
            let mut bytes = json.into_bytes();
            bytes.push(b'\n');
            if tx.send(bytes).await.is_err() { break; }
        }
    }
}


// ---------------------------------------------------------------------------
// Transport trait
// ---------------------------------------------------------------------------

/// Common interface for ACP transports.
#[async_trait::async_trait]
pub trait Transport: Send {
    /// Get a cloneable sender handle.
    fn sender(&self) -> TransportSender;
    /// Receive the next incoming message. Returns `None` when the source closes.
    async fn recv(&mut self) -> Option<IncomingMessage>;
}

#[async_trait::async_trait]
impl Transport for StdioTransport {
    fn sender(&self) -> TransportSender {
        self.sender.clone()
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.incoming_rx.recv().await
    }
}

#[async_trait::async_trait]
impl Transport for InProcessTransport {
    fn sender(&self) -> TransportSender {
        self.sender.clone()
    }
    async fn recv(&mut self) -> Option<IncomingMessage> {
        self.incoming_rx.recv().await
    }
}

pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let raw: RawJsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(raw.method.as_deref(), Some("initialize"));
        assert_eq!(raw.id, Some(JsonRpcId::Number(1)));
    }

    #[test]
    fn parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"session/cancel","params":{"id":"abc"}}"#;
        let raw: RawJsonRpcMessage = serde_json::from_str(json).unwrap();
        assert_eq!(raw.method.as_deref(), Some("session/cancel"));
        assert!(raw.id.is_none());
    }

    #[test]
    fn serialize_response() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Number(1),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""jsonrpc":"2.0""#));
        assert!(json.contains(r#""id":1"#));
    }

    #[tokio::test]
    async fn inprocess_transport_echo() {
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);

        let mut transport = InProcessTransport::new(in_rx, out_tx);
        let sender = transport.get_sender();

        // Send a request message via the input channel.
        let msg = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n".to_vec();
        in_tx.send(msg).await.unwrap();

        // The transport should produce a parsed IncomingMessage.
        let incoming = transport.recv().await.expect("expected a message");
        match incoming {
            IncomingMessage::Request { method, .. } => assert_eq!(method, "initialize"),
            other => panic!("unexpected message: {:?}", other),
        }

        // Send a response via the sender and check it arrives on the output channel.
        let resp = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        sender.send_response(JsonRpcId::Number(1), resp).await.unwrap();

        let raw_out = out_rx.recv().await.expect("expected response bytes");
        let text = String::from_utf8(raw_out).unwrap();
        assert!(text.contains(r#""id":1"#));
    }
}
