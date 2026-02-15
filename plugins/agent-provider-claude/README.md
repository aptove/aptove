# agent-provider-claude

Anthropic Claude LLM provider for the Aptove agent.

## Architecture

Implements the `agent_core::plugin::LlmProvider` trait against the [Anthropic Messages API](https://docs.anthropic.com/en/api/messages).

```
┌─────────────────────────────────────────┐
│          ClaudeProvider                 │
│                                         │
│  ┌───────────┐   ┌──────────────────┐   │
│  │ api_key   │   │ build_request    │   │
│  │ model     │   │ _body()          │   │
│  │ base_url  │   │                  │   │
│  │ client    │   │ Messages → JSON  │   │
│  │ (reqwest) │   │ Tools → JSON     │   │
│  └───────────┘   └──────────────────┘   │
│                                         │
│  ┌──────────────────────────────────┐   │
│  │ LlmProvider::complete()         │   │
│  │                                  │   │
│  │  POST /v1/messages               │   │
│  │  x-api-key + anthropic-version   │   │
│  │  Parse content blocks            │   │
│  │  Extract tool_use blocks         │   │
│  │  Map stop_reason                 │   │
│  │  Report usage (input/output)     │   │
│  └──────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

## Design

### Message Conversion

The internal `Message` type maps to Anthropic's format:

| Internal | Anthropic API |
|---|---|
| `Role::System` | Extracted as top-level `system` parameter |
| `Role::User` | `{ "role": "user", "content": "…" }` |
| `Role::Assistant` (text) | `{ "role": "assistant", "content": "…" }` |
| `Role::Assistant` (tool calls) | `{ "role": "assistant", "content": [{ "type": "tool_use", … }] }` |
| `Role::Tool` | `{ "role": "user", "content": [{ "type": "tool_result", … }] }` |

Note: Anthropic represents tool results as `user` messages with `tool_result` content blocks.

### Tool Use

Tools are sent as Anthropic's native `tools` parameter with `input_schema` for each tool definition. Tool calls come back as `tool_use` content blocks with an `id`, `name`, and `input` object.

### Token Counting

Uses a character-based heuristic (~3.5 chars/token for English) since Anthropic doesn't publish a public tokenizer library. This is intentionally conservative for context window management.

### Model Info

Model context limits are inferred from the model name:

| Model Family | Context | Max Output |
|---|---|---|
| Opus | 200K | 32K |
| Sonnet | 200K | 64K |
| Haiku | 200K | 8K |

### Streaming

The request body sets `"stream": true`. Full SSE parsing is a TODO — currently falls back to reading the complete response. The `StreamCallback` is invoked once with the full text content when available.

### Custom Base URL

Supports overriding `base_url` for Anthropic-compatible proxies or enterprise endpoints.

## Configuration

```toml
[providers.claude]
api_key = "sk-ant-..."   # or set ANTHROPIC_API_KEY env var
model = "claude-sonnet-4-20250514"
# base_url = "https://api.anthropic.com"
```
