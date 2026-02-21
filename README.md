# Aptove — ACP AI Coding Agent

A Rust CLI AI coding agent that speaks the [Agent-Client Protocol (ACP)](https://agentclientprotocol.org), supports multiple LLM providers, and connects to external MCP servers for tool use.

## Architecture

```
agent-cli           Binary entry point ("aptove"), CLI parsing, modes
agent-core          Transport, sessions, context window, agent loop, plugins
agent-provider-*    LLM provider implementations (Claude, Gemini, OpenAI)
agent-mcp-bridge    MCP client that connects to external tool servers
```

### Core Modules

| Module | Purpose |
|---|---|
| `transport` | JSON-RPC 2.0 over stdin/stdout (ACP stdio mode) |
| `session` | Session lifecycle, per-session context and cancellation |
| `context` | Token-aware sliding context window with auto-compaction |
| `agent_loop` | Agentic tool-call loop (prompt → LLM → tools → repeat) |
| `plugin` | `LlmProvider` and `Plugin` traits, `PluginHost` registry |
| `config` | TOML config loading, API key resolution, validation |
| `system_prompt` | Prompt templates with mode/project overrides |
| `retry` | Exponential backoff with jitter for API calls |
| `persistence` | Save/load sessions to `~/.local/share/Aptove/sessions/` |

## Quick Start

### Build

```sh
cd agent
cargo build
```

### Run

```sh
# Generate a config file
cargo run --bin aptove -- config init

# Edit the config file to add your API key (see Configuration section below)

# Start in ACP stdio mode (for use with bridge/clients)
cargo run --bin aptove -- run

# Start embedded bridge + agent in a single process
cargo run --bin aptove -- serve --port 8765

# Start interactive chat mode
cargo run --bin aptove -- chat
```

### Configuration

The config file location follows the OS-native convention via Rust's `dirs::config_dir()`:

| OS | Config path |
|---|---|
| **macOS** | `~/Library/Application Support/Aptove/config.toml` |
| **Linux** | `~/.config/Aptove/config.toml` |
| **Windows** | `%APPDATA%\Aptove\config.toml` |

```toml
provider = "claude"

[providers.claude]
# api_key = "sk-ant-..."  # Or set ANTHROPIC_API_KEY env var
model = "claude-sonnet-4-20250514"

[providers.gemini]
# api_key = "..."  # Or set GOOGLE_API_KEY env var
model = "gemini-2.5-pro"

[providers.openai]
# api_key = "sk-..."  # Or set OPENAI_API_KEY env var
model = "gpt-4o"

[[mcp_servers]]
name = "filesystem"
command = "mcp-server-filesystem"
args = ["/path/to/allowed/dir"]

[agent]
max_tool_iterations = 25
```

API keys can be set via environment variables: `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`, `OPENAI_API_KEY`.

### Test

```sh
cargo test
```

## Providers

- **Claude** — Anthropic Messages API with tool use
- **Gemini** — Google Generative Language API with function calling
- **OpenAI** — Chat Completions API with function calling (also supports compatible endpoints)

## ACP Protocol

Aptove implements the [ACP protocol](https://agentclientprotocol.org) over stdio:

- `initialize` → Returns agent info and capabilities
- `session/new` → Creates a session with context window
- `session/prompt` → Runs the agentic tool loop, streams updates
- `session/cancel` → Cancels an in-flight prompt

Use with [bridge](../bridge/) for WebSocket↔stdio bridging.

## License

Apache-2.0
