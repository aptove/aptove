//! MCP Bridge
//!
//! Connects to external MCP (Model Context Protocol) servers as an MCP client.
//! Spawns MCP servers as child processes, performs initialization handshake,
//! discovers tools, and forwards tool calls.

use std::collections::HashMap;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use agent_core::config::McpServerConfig;
use agent_core::plugin::{Plugin, ToolCallRequest, ToolCallResult, ToolDefinition};

// ---------------------------------------------------------------------------
// MCP Client
// ---------------------------------------------------------------------------

/// A connection to a single MCP server process.
struct McpConnection {
    /// Server config.
    config: McpServerConfig,
    /// Child process handle.
    child: Child,
    /// Writer to child's stdin.
    stdin: tokio::process::ChildStdin,
    /// Reader from child's stdout.
    stdout: BufReader<tokio::process::ChildStdout>,
    /// Discovered tools.
    tools: Vec<ToolDefinition>,
    /// Next JSON-RPC request id.
    next_id: i64,
}

impl McpConnection {
    /// Spawn an MCP server process and perform initialization.
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        info!(name = %config.name, command = %config.command, "spawning MCP server");

        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()); // MCP server logs go to our stderr

        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "failed to spawn MCP server '{}' (command: {})",
                config.name, config.command
            )
        })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stdout = BufReader::new(stdout);

        let mut conn = Self {
            config: config.clone(),
            child,
            stdin,
            stdout,
            tools: Vec::new(),
            next_id: 1,
        };

        // MCP initialize handshake
        conn.initialize().await?;
        conn.discover_tools().await?;

        info!(
            name = %config.name,
            tools = conn.tools.len(),
            "MCP server connected"
        );

        Ok(conn)
    }

    /// Send MCP initialize request.
    async fn initialize(&mut self) -> Result<()> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "aptove-agent",
                    "version": "0.1.0"
                }
            }
        });

        let response = self.send_request(request).await?;
        debug!(name = %self.config.name, response = %response, "MCP initialize response");

        // Send initialized notification
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        self.send_notification(notification).await?;

        Ok(())
    }

    /// Discover available tools from the MCP server.
    async fn discover_tools(&mut self) -> Result<()> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/list",
            "params": {}
        });

        let response = self.send_request(request).await?;

        if let Some(tools) = response
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
        {
            for tool in tools {
                let name = tool
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let description = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string();
                let parameters = tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or(serde_json::json!({"type": "object"}));

                self.tools.push(ToolDefinition {
                    name,
                    description,
                    parameters,
                });
            }
        }

        Ok(())
    }

    /// Call a tool on this MCP server.
    async fn call_tool(&mut self, name: &str, arguments: &serde_json::Value) -> Result<String> {
        // MCP servers expect arguments to be an object, not null
        // If arguments is null, send an empty object instead
        let args = if arguments.is_null() {
            serde_json::json!({})
        } else {
            arguments.clone()
        };

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_request_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": args
            }
        });

        let response = self.send_request(request).await?;

        // Extract tool result content
        let content = response
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| {
                response
                    .get("result")
                    .map(|r| r.to_string())
                    .unwrap_or_default()
            });

        Ok(content)
    }

    /// Send a JSON-RPC request and wait for a response.
    async fn send_request(&mut self, request: serde_json::Value) -> Result<serde_json::Value> {
        let line = format!("{}\n", serde_json::to_string(&request)?);
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("failed to write to MCP server stdin")?;
        self.stdin.flush().await?;

        // Read response line
        let mut response_line = String::new();
        self.stdout
            .read_line(&mut response_line)
            .await
            .context("failed to read from MCP server stdout")?;

        let response: serde_json::Value =
            serde_json::from_str(response_line.trim()).context("invalid JSON from MCP server")?;

        // Check for JSON-RPC error
        if let Some(err) = response.get("error") {
            anyhow::bail!("MCP server error: {}", err);
        }

        Ok(response)
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn send_notification(&mut self, notification: serde_json::Value) -> Result<()> {
        let line = format!("{}\n", serde_json::to_string(&notification)?);
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    fn next_request_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Check if the MCP server process is still running.
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

// ---------------------------------------------------------------------------
// MCP Bridge Plugin
// ---------------------------------------------------------------------------

/// Plugin that manages MCP server connections and routes tool calls.
pub struct McpBridge {
    connections: HashMap<String, Mutex<McpConnection>>,
    /// Map tool name → MCP server name (for routing).
    tool_routing: HashMap<String, String>,
    /// All discovered tool definitions.
    all_tool_defs: Vec<ToolDefinition>,
}

impl McpBridge {
    pub fn new() -> Self {
        Self {
            connections: HashMap::new(),
            tool_routing: HashMap::new(),
            all_tool_defs: Vec::new(),
        }
    }

    /// Connect to all configured MCP servers.
    pub async fn connect_all(&mut self, configs: &[McpServerConfig]) -> Result<()> {
        for config in configs {
            match McpConnection::connect(config).await {
                Ok(conn) => {
                    // Register tool routing and collect tool definitions
                    for tool in &conn.tools {
                        self.tool_routing
                            .insert(tool.name.clone(), config.name.clone());
                        self.all_tool_defs.push(tool.clone());
                    }
                    self.connections
                        .insert(config.name.clone(), Mutex::new(conn));
                }
                Err(e) => {
                    warn!(
                        name = %config.name,
                        err = %e,
                        "failed to connect to MCP server, skipping"
                    );
                }
            }
        }
        Ok(())
    }

    /// Get all discovered tool definitions.
    pub fn all_tools(&self) -> Vec<ToolDefinition> {
        self.all_tool_defs.clone()
    }

    /// List connected server names, their alive status, and tool count.
    pub fn server_info(&self) -> Vec<(String, bool, Vec<String>)> {
        let mut result = Vec::new();
        for (server_name, conn) in &self.connections {
            let alive = matches!(conn.try_lock(), Ok(_)); // alive if we can lock it
            let tool_names: Vec<String> = self
                .all_tool_defs
                .iter()
                .filter(|t| self.tool_routing.get(&t.name) == Some(server_name))
                .map(|t| t.name.clone())
                .collect();
            result.push((server_name.clone(), alive, tool_names));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }

    /// Execute a tool call by routing to the correct MCP server.
    pub async fn execute_tool(&self, request: ToolCallRequest) -> ToolCallResult {
        let server_name = match self.tool_routing.get(&request.name) {
            Some(name) => name,
            None => {
                return ToolCallResult {
                    tool_call_id: request.id.clone(),
                    content: format!("Tool '{}' not found in any MCP server", request.name),
                    is_error: true,
                };
            }
        };

        let conn = match self.connections.get(server_name) {
            Some(c) => c,
            None => {
                return ToolCallResult {
                    tool_call_id: request.id.clone(),
                    content: format!("MCP server '{}' not connected", server_name),
                    is_error: true,
                };
            }
        };

        let mut conn = conn.lock().await;

        // Check if the server is still alive
        if !conn.is_alive() {
            error!(server = %server_name, "MCP server has crashed");
            return ToolCallResult {
                tool_call_id: request.id.clone(),
                content: format!("MCP server '{}' has crashed", server_name),
                is_error: true,
            };
        }

        match conn.call_tool(&request.name, &request.arguments).await {
            Ok(content) => ToolCallResult {
                tool_call_id: request.id.clone(),
                content,
                is_error: false,
            },
            Err(e) => ToolCallResult {
                tool_call_id: request.id.clone(),
                content: format!("Tool execution error: {}", e),
                is_error: true,
            },
        }
    }

    /// Shut down all MCP server connections.
    pub async fn shutdown(&mut self) {
        for (name, conn) in self.connections.drain() {
            let mut conn = conn.into_inner();
            info!(name = %name, "shutting down MCP server");
            let _ = conn.child.kill().await;
        }
    }
}

impl Default for McpBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for McpBridge {
    fn name(&self) -> &str {
        "mcp-bridge"
    }

    async fn on_init(&mut self, _config: &serde_json::Value) -> Result<()> {
        // MCP servers are connected separately via connect_all()
        Ok(())
    }

    async fn on_shutdown(&self) -> Result<()> {
        // Note: actual shutdown needs &mut self, handled separately
        Ok(())
    }
}
