# agent-cli

Binary entry point for the Aptove agent. Compiles to the `aptove` executable.

## Architecture

```
                        ┌──────────────┐
                        │   main.rs    │
                        │  (clap CLI)  │
                        └──────┬───────┘
                               │
              ┌────────────────┼─────────────────┐
              │                │                 │
        ┌─────┴─────┐   ┌─────┴─────┐   ┌───────┴──────┐
        │  ACP Mode │   │ Chat Mode │   │ Config Cmds  │
        │  (run)    │   │  (chat)   │   │ (show/init)  │
        └─────┬─────┘   └─────┬─────┘   └──────────────┘
              │                │
        ┌─────┴──────────────┐ │
        │ StdioTransport     │ │
        │ + message dispatch │ │
        └─────┬──────────────┘ │
              │                │
        ┌─────┴────────────────┴──┐
        │       AgentState        │
        │  ┌────────────────────┐ │
        │  │ AgentConfig        │ │
        │  │ PluginHost (RwLock)│ │
        │  │ SessionManager     │ │
        │  │ TransportSender    │ │
        │  └────────────────────┘ │
        └─────────────────────────┘
```

## Modes

### `aptove run` (default)

ACP stdio mode. Designed to be launched as a child process by the [bridge](../../bridge/) or any ACP-compliant client.

- Creates a `StdioTransport` that reads JSON-RPC from stdin and writes to stdout.
- Wraps all shared state in an `Arc<AgentState>` so each incoming request is handled in its own tokio task.
- Implements the following ACP methods:
  - **`initialize`** → Returns agent info (`Aptove v0.1.0`), protocol version, and capabilities.
  - **`session/new`** → Creates a session, injects the system prompt, returns the session ID.
  - **`session/prompt`** → Extracts the prompt text, runs `agent_loop::run_agent_loop()`, streams progress via `session/update` notifications, returns the final result with token usage.
  - **`session/cancel`** → Triggers the session's `CancellationToken`.
- Unrecognized methods get a `METHOD_NOT_FOUND` JSON-RPC error.

### `aptove chat`

Interactive REPL for direct terminal use.

- Validates the config on startup and displays provider/model info.
- Supports slash commands: `/help`, `/clear`, `/context`, `/model`, `/provider`, `/usage`, `/sessions`, `/quit`.
- Prompt handling is currently a placeholder — production chat mode will wire into the same agent loop used by ACP mode.

### `aptove config show|init`

Configuration management.

- **`show`** — Serializes the resolved config as pretty TOML to stdout.
- **`init`** — Writes a commented sample config to `~/.config/acp-agent/config.toml` if it doesn't already exist.

## Startup Flow

1. Initialize `tracing_subscriber` with stderr output (stdout is for JSON-RPC).
2. Parse CLI args with clap.
3. Load `AgentConfig` from `--config` path or the default location.
4. Dispatch to the selected mode.

## Plugin Wiring

`setup_plugins()` creates a `PluginHost` and registers LLM providers based on available API keys. Currently a placeholder — provider registration will be wired once the providers are integrated into the startup path.

## Dependencies

Uses `agent-core` for all business logic. Direct dependencies are limited to CLI concerns: `clap` (arg parsing), `tracing-subscriber` (logging setup), `toml` (config serialization), `serde_json` (ACP message construction).
