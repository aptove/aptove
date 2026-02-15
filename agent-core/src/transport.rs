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
        let msg = OutgoingMessage::Response(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        });
        self.tx.send(msg).await.context("stdout writer closed")?;
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
    pub fn sender(&self) -> TransportSender {
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
            if stdout.write_all(line.as_bytes()).await.is_err() {
                error!("stdout write failed");
                break;
            }
            if stdout.flush().await.is_err() {
                error!("stdout flush failed");
                break;
            }
        }
    }
}

/// Standard JSON-RPC error codes.
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
}
