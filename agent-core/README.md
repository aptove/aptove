# agent-core

Core library for the Aptove ACP agent. Every other crate in the workspace depends on this one.

## Architecture

`agent-core` is a pure library crate — it has no binary, no I/O policy opinions, and no provider-specific code. It exposes traits and data structures that the CLI, providers, and MCP bridge plug into.

```
┌─────────────────────────────────────────────────────────┐
│                      agent-core                         │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌────────────────────────┐ │
│  │transport │  │ session  │  │      agent_loop        │ │
│  │(stdio)   │──│ manager  │──│ prompt→LLM→tools→loop  │ │
│  └──────────┘  └────┬─────┘  └────────────┬───────────┘ │
│                     │                     │             │
│               ┌─────┴──────┐        ┌─────┴──────┐      │
│               │  context   │        │   plugin   │      │
│               │  (window)  │        │ (traits)   │      │
│               └────────────┘        └────────────┘      │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐ ┌──────────┐  │
│  │  config  │  │  retry   │  │ persist  │ │sys_prompt│  │
│  └──────────┘  └──────────┘  └──────────┘ └──────────┘  │
└─────────────────────────────────────────────────────────┘
```

## Modules

### `transport` — ACP Stdio Transport

JSON-RPC 2.0 over stdin/stdout. This is the wire format defined by the [Agent-Client Protocol](https://agentclientprotocol.org).

- `StdioTransport` spawns two background tokio tasks: one reads newline-delimited JSON from stdin, the other writes to stdout.
- `TransportSender` is a cheaply cloneable handle that any subsystem can use to push responses or notifications to the client.
- `IncomingMessage` classifies each parsed frame as a `Request` (has `id`, expects response) or `Notification` (fire-and-forget).
- All logging goes to stderr — stdout is reserved exclusively for the JSON-RPC channel.

### `plugin` — Trait System

The provider-agnostic interface layer.

- **`LlmProvider`** — the central async trait every provider implements: `complete()`, `count_tokens()`, `model_info()`. Accepts an optional `StreamCallback` for real-time token streaming.
- **`Plugin`** — lifecycle hooks (`on_init`, `on_shutdown`) for any component that needs startup/teardown.
- **`PluginHost`** — the runtime registry. Stores providers in `Arc<RwLock<Box<dyn LlmProvider>>>` keyed by name. Tracks the active provider and tool definitions from MCP servers. Supports hot-switching the active provider.
- **Supporting types**: `Message`, `Role`, `MessageContent` (text / tool-calls / tool-result), `ToolDefinition`, `ToolCallRequest`, `ToolCallResult`, `LlmResponse`, `TokenUsage`, `StopReason`, `StreamEvent`.

Design choice: providers are behind `Arc<RwLock<…>>` so the agent loop can hold a read guard during a long-running `complete()` call while the chat REPL can still switch providers from another task.

### `session` — Session Management

Maps ACP's session lifecycle onto in-memory state.

- `Session` owns a `ContextWindow`, a `CancellationToken` (from `tokio-util`), provider/model metadata, and a busy flag.
- `SessionManager` stores sessions in `Arc<RwLock<HashMap<SessionId, Arc<Mutex<Session>>>>>` for concurrent access from multiple request handlers.
- Supports create / get / cancel / remove / list operations.

### `context` — Context Window

Token-aware sliding window over the conversation history.

- Maintains a parallel `Vec<usize>` of per-message token counts alongside the messages themselves.
- `add_message()` auto-compacts when `total_tokens > max_tokens` — removes the oldest non-system messages until the window is at 80% capacity.
- `TokenCounter` is a pluggable `Box<dyn Fn(&str) -> usize>` so providers can inject their exact tokenizer. Default: `ceil(chars / 4)`.
- `clear()` preserves system messages. `compact()` drops oldest user/assistant turns.

### `agent_loop` — Agentic Tool Loop

The core prompt→LLM→tools→loop cycle.

1. Check cancellation token.
2. Check iteration limit (default 25).
3. Call `provider.complete()` with current messages + tools.
4. Accumulate token usage.
5. If the LLM returned text only → done.
6. If the LLM returned tool calls → execute each via the `ToolExecutor` callback → append results → go to 1.

Stream events and tool progress are emitted as `session/update` JSON-RPC notifications through the transport sender.

### `config` — Configuration

TOML-based config at `~/.config/acp-agent/config.toml`.

- `AgentConfig` with nested `ProvidersConfig` (claude/gemini/openai), `McpServerConfig` list, `AgentSettings` (max iterations, retry policy), `SystemPromptConfig`.
- `resolve_api_key()` checks config first, then falls back to environment variables (`ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`, `OPENAI_API_KEY`).
- `validate()` checks that the active provider has an API key and that MCP server commands exist on `PATH` (via the `which` crate).
- `sample_config()` generates a commented template for `config init`.

### `system_prompt` — System Prompt Manager

Layered prompt resolution with template variables.

- Priority: project override (`.acp-agent/system_prompt.md`) > mode-specific > default.
- Template variables: `{{provider}}`, `{{model}}`, `{{tools}}` are substituted at build time.
- `build_message()` returns a ready-to-use `Message { role: System, … }`.

### `retry` — Exponential Backoff

Generic retry executor for transient API errors.

- `RetryPolicy` with configurable max retries, base delay, max delay, and backoff multiplier.
- `ErrorKind` classification: `RateLimit` (429 with optional Retry-After), `ServerError` (5xx), `NetworkError`, `MalformedResponse`, `Fatal`.
- `with_retry()` is a generic async function that wraps any fallible operation.

### `persistence` — Session Store

Save/load sessions as JSON files.

- `SessionStore` trait with `save`, `load`, `list`, `delete`.
- `FileSessionStore` at `~/.local/share/acp-agent/sessions/`. Sessions are pretty-printed JSON. List is sorted newest-first and tolerates corrupt files.

## Key Design Decisions

1. **No ACP SDK runtime dependency** — the core uses `agent-client-protocol-schema` (types only) and implements the JSON-RPC dispatch itself. This avoids the official SDK's `!Send` futures requirement.
2. **Trait-based providers** — `LlmProvider` is `Send + Sync` so it works naturally with tokio's multi-threaded runtime.
3. **Compaction over truncation** — the context window removes complete messages oldest-first rather than truncating mid-message, preserving conversation coherence.
4. **Cancellation via CancellationToken** — cooperative cancellation at the agent loop level; each iteration checks before calling the LLM.

## Tests

37 unit tests covering all modules. Run with:

```sh
cargo test -p agent-core
```
