# agent-provider-gemini

Google Gemini LLM provider for the Aptove agent.

## Architecture

Implements the `agent_core::plugin::LlmProvider` trait against the [Google Generative Language API](https://ai.google.dev/api).

```
┌─────────────────────────────────────────┐
│          GeminiProvider                 │
│                                         │
│  ┌───────────┐   ┌──────────────────┐   │
│  │ api_key   │   │ build_request    │   │
│  │ model     │   │ _body()          │   │
│  │ client    │   │                  │   │
│  │ (reqwest) │   │ Messages → JSON  │   │
│  └───────────┘   │ Tools → funcDecl │   │
│                  └──────────────────┘   │
│                                         │
│  ┌──────────────────────────────────┐   │
│  │ LlmProvider::complete()         │   │
│  │                                  │   │
│  │  POST /v1beta/models/{m}:       │   │
│  │       generateContent?key=…     │   │
│  │  Parse candidates[0].content    │   │
│  │  Extract functionCall parts     │   │
│  │  Read usageMetadata             │   │
│  └──────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

## Design

### Message Conversion

The internal `Message` type maps to Gemini's format:

| Internal | Gemini API |
|---|---|
| `Role::System` | `systemInstruction` top-level parameter |
| `Role::User` | `{ "role": "user", "parts": [{ "text": "…" }] }` |
| `Role::Assistant` | `{ "role": "model", "parts": [{ "text": "…" }] }` |
| `Role::Tool` | `{ "role": "user", "parts": [{ "functionResponse": { … } }] }` |

Note: Gemini uses `"model"` instead of `"assistant"` as the role name.

### Function Calling

Tools are sent as `functionDeclarations` inside a `tools` array. Gemini returns `functionCall` parts in the response, each containing a `name` and `args` object. Since Gemini doesn't assign unique IDs to function calls, the provider generates a UUID v4 for each one to maintain compatibility with the internal `ToolCallRequest.id` field.

### Token Counting

Uses a character-based heuristic (~4 chars/token) similar to SentencePiece tokenization. Gemini also returns exact token counts in `usageMetadata` which are reported in the `TokenUsage` response.

### Model Info

| Model Family | Context | Max Output |
|---|---|---|
| Gemini 2.5 Pro | 1M | 65K |
| Gemini 2.5 Flash | 1M | 65K |
| Gemini 2.0 | 1M | 8K |

### API Key in URL

Unlike Claude and OpenAI which use headers, Gemini passes the API key as a `key` query parameter. This is the standard approach for the Generative Language API.

## Configuration

```toml
[providers.gemini]
api_key = "..."   # or set GOOGLE_API_KEY env var
model = "gemini-2.5-pro"
```
