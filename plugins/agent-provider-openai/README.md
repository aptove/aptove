# agent-provider-openai

OpenAI LLM provider for the Aptove agent.

## Architecture

Implements the `agent_core::plugin::LlmProvider` trait against the [OpenAI Chat Completions API](https://platform.openai.com/docs/api-reference/chat).

```
┌─────────────────────────────────────────┐
│         OpenAiProvider                  │
│                                         │
│  ┌───────────┐   ┌──────────────────┐   │
│  │ api_key   │   │ build_request    │   │
│  │ model     │   │ _body()          │   │
│  │ base_url  │   │                  │   │
│  │ client    │   │ Messages → JSON  │   │
│  │ (reqwest) │   │ Tools → function │   │
│  └───────────┘   └──────────────────┘   │
│                                         │
│  ┌──────────────────────────────────┐   │
│  │ LlmProvider::complete()         │   │
│  │                                  │   │
│  │  POST {base_url}/v1/chat/       │   │
│  │       completions               │   │
│  │  Authorization: Bearer {key}     │   │
│  │  Parse choices[0].message       │   │
│  │  Extract tool_calls array       │   │
│  │  Map finish_reason              │   │
│  │  Report usage (prompt/compl.)   │   │
│  └──────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

## Design

### Message Conversion

The internal `Message` type maps directly to OpenAI's format:

| Internal | OpenAI API |
|---|---|
| `Role::System` | `{ "role": "system", "content": "…" }` |
| `Role::User` | `{ "role": "user", "content": "…" }` |
| `Role::Assistant` (text) | `{ "role": "assistant", "content": "…" }` |
| `Role::Assistant` (tool calls) | `{ "role": "assistant", "tool_calls": [{ "type": "function", … }] }` |
| `Role::Tool` | `{ "role": "tool", "tool_call_id": "…", "content": "…" }` |

OpenAI's message format is the most direct mapping — roles match 1:1 and tool results have a dedicated `tool` role.

### Function Calling

Tools are sent as `tools` with `type: "function"` wrappers. Each tool call in the response has an `id`, a `function.name`, and `function.arguments` (a JSON string that must be parsed). The `arguments` string is parsed back into `serde_json::Value` for the internal representation.

### Finish Reason Mapping

| OpenAI `finish_reason` | Internal `StopReason` |
|---|---|
| `"stop"` | `EndTurn` |
| `"tool_calls"` | `ToolUse` |
| `"length"` | `MaxTokens` |
| (fallback with tool calls) | `ToolUse` |

### Token Counting

Uses a character-based heuristic (~4 chars/token). A future improvement is to integrate `tiktoken-rs` for exact BPE counting. OpenAI returns exact counts in the response `usage` object (`prompt_tokens`, `completion_tokens`, `total_tokens`).

### Model Info

| Model | Context | Max Output |
|---|---|---|
| GPT-4o | 128K | 16K |
| GPT-4o-mini | 128K | 16K |
| GPT-4 Turbo | 128K | 4K |
| GPT-4 | 8K | 4K |
| o1 / o1-preview | 200K | 100K |
| o3-mini | 200K | 100K |

### Compatible Endpoints

The `base_url` field (default: `https://api.openai.com`) allows targeting any OpenAI-compatible API: Together AI, Groq, Fireworks, local vLLM/Ollama instances, Azure OpenAI, etc.

## Configuration

```toml
[providers.openai]
api_key = "sk-..."   # or set OPENAI_API_KEY env var
model = "gpt-4o"
# base_url = "https://api.openai.com"   # or any compatible endpoint
```
