# agent-mcp-bridge

MCP (Model Context Protocol) client bridge for the Aptove agent. Connects to external MCP servers for tool use.

## Architecture

The bridge manages the full lifecycle of MCP server processes: spawn → initialize → discover tools → route tool calls → shutdown.

```
                    ┌──────────────────────┐
                    │      McpBridge       │
                    │    (Plugin impl)     │
                    │                      │
                    │  connections: Map     │
                    │  tool_routing: Map    │
                    └──────────┬───────────┘
                               │
          ┌────────────────────┼────────────────────┐
          │                    │                    │
  ┌───────┴────────┐  ┌───────┴────────┐  ┌───────┴────────┐
  │ McpConnection  │  │ McpConnection  │  │ McpConnection  │
  │ "filesystem"   │  │   "bash"       │  │  "browser"     │
  │                │  │                │  │                │
  │ child process  │  │ child process  │  │ child process  │
  │ stdin (write)  │  │ stdin (write)  │  │ stdin (write)  │
  │ stdout (read)  │  │ stdout (read)  │  │ stdout (read)  │
  │ tools: [...]   │  │ tools: [...]   │  │ tools: [...]   │
  └────────────────┘  └────────────────┘  └────────────────┘
```

## Design

### Connection Lifecycle

1. **Spawn**: Each MCP server is started as a child process with `stdin`/`stdout` piped and `stderr` inherited (so server logs flow through to the agent's log output).
2. **Initialize**: Sends the MCP `initialize` request with protocol version `2024-11-05` and client info (`aptove-agent v0.1.0`), then the `notifications/initialized` notification.
3. **Discover Tools**: Calls `tools/list` and maps each tool's `inputSchema` to the internal `ToolDefinition` type.
4. **Ready**: Tools are registered in the `tool_routing` map (tool name → server name) for O(1) routing.

### Tool Call Routing

When the agent loop calls `execute_tool()`:

1. Look up the tool name in `tool_routing` to find the target MCP server.
2. Acquire the `Mutex` on that server's `McpConnection`.
3. Check `is_alive()` — if the server process has crashed, return an error result immediately.
4. Send a `tools/call` JSON-RPC request with the tool name and arguments.
5. Parse the response, extracting text content from the result's `content` array.
6. Return a `ToolCallResult` (success or error).

### JSON-RPC Transport

All communication with MCP servers uses newline-delimited JSON-RPC 2.0 over stdio, matching the MCP specification:

- `send_request()` → write JSON + newline to stdin, read one line from stdout, parse as JSON.
- `send_notification()` → write JSON + newline to stdin, no response expected.
- Request IDs are sequential integers starting from 1 per connection.

### Error Handling

- **Server spawn failure**: Logged as a warning, server is skipped. Other servers still connect.
- **Server crash**: Detected via `child.try_wait()`. Returns an error `ToolCallResult` to the LLM so it can decide how to proceed.
- **JSON-RPC errors**: The `error` field in responses is checked and propagated as `anyhow::Error`.

### Concurrency

Each `McpConnection` is behind a `tokio::sync::Mutex` because MCP servers use a synchronous request-response protocol over stdin/stdout — only one request can be in flight at a time per server. Multiple servers can be called concurrently since each has its own mutex.

### Plugin Trait

`McpBridge` implements `agent_core::plugin::Plugin` for lifecycle integration. The actual initialization is done via `connect_all()` which takes the list of `McpServerConfig` from the agent config.

## Configuration

MCP servers are defined in the agent config:

```toml
[[mcp_servers]]
name = "filesystem"
command = "mcp-server-filesystem"
args = ["/path/to/allowed/directory"]
transport = "stdio"

[[mcp_servers]]
name = "bash"
command = "mcp-server-bash"
env = { SHELL = "/bin/zsh" }
```

Each entry specifies:

| Field | Description |
|---|---|
| `name` | Display name used for logging and tool routing |
| `command` | Executable to spawn (must be on PATH) |
| `args` | Command-line arguments passed to the server |
| `transport` | Protocol transport — currently only `"stdio"` |
| `env` | Additional environment variables for the server process |
