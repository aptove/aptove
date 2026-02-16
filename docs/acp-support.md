# ACP Protocol Support

This document outlines the Agent-Client Protocol (ACP) methods supported by the Aptove agent.

## Overview

The Agent-Client Protocol (ACP) defines the communication interface between clients (mobile apps, CLIs) and agents. The protocol uses JSON-RPC 2.0 over stdio or WebSocket transport.

## Session Methods

### Core Methods (Required)

All agents **MUST** support these baseline methods:

| Method | Type | Status | Description |
|--------|------|--------|-------------|
| **`session/new`** | Request | ✅ Implemented | Create a new session |
| **`session/prompt`** | Request | ✅ Implemented | Send a user message and get agent response |
| **`session/cancel`** | Notification | ❌ Not implemented | Cancel ongoing operations for a session |
| **`session/update`** | Notification | ✅ Implemented | Stream updates from agent to client (agent→client) |

### Optional Methods (Feature-Gated)

These methods are optional and indicated by agent capabilities:

| Method | Feature Flag | Status | Description |
|--------|--------------|--------|-------------|
| **`session/load`** | `loadSession` | ❌ Not implemented | Load an existing session with message history |
| **`session/list`** | `unstable_session_list` | ❌ Not implemented | List all existing sessions (with pagination) |
| **`session/fork`** | `unstable_session_fork` | ❌ Not implemented | Fork an existing session into a new one |
| **`session/resume`** | `unstable_session_resume` | ❌ Not implemented | Resume a session without loading message history |
| **`session/set_mode`** | Session modes support | ❌ Not implemented | Set the mode for a session (e.g., code, plan, chat) |
| **`session/set_config_option`** | Session config support | ❌ Not implemented | Set a configuration option for a session |
| **`session/set_model`** | `unstable_session_model` | ❌ Not implemented | Change the LLM model for a session |

---

## Method Details

### 1. `session/new` ✅

Creates a new session or reuses an existing one (mobile apps).

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "session/new",
  "params": {
    "cwd": "/path/to/directory",
    "mcpServers": [],
    "_meta": {
      "sessionId": "optional-uuid-for-mobile-apps"
    }
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "sessionId": "abc-123-...",
    "modes": { /* optional */ },
    "config": { /* optional */ }
  }
}
```

**Aptove Implementation Notes:**
- **Mobile Apps**: Pass `_meta.sessionId` to reuse workspace across app restarts
  - First connection: No sessionId → agent generates UUID → returns it → app saves it
  - Subsequent connections: Send saved sessionId → agent reuses workspace → clears conversation
- **CLI Mode**: Uses "default" workspace
- **Legacy**: Supports `device_id` parameter for device binding (deprecated)

**Files:**
- Implementation: `agent-cli/src/main.rs` (`handle_session_new`)
- Session creation: `agent-core/src/session.rs` (`SessionManager`)

---

### 2. `session/prompt` ✅

Sends a user message to the agent and receives a response.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/prompt",
  "params": {
    "sessionId": "abc-123",
    "prompt": [
      { "type": "text", "text": "Hello, how are you?" }
    ]
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "stopReason": "end_turn"
  }
}
```

**Agent Streaming:**
Agent streams responses via `session/update` notifications during processing:

```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "abc-123",
    "update": {
      "type": "agentMessageChunk",
      "content": { "type": "text", "text": "Hello! I'm doing well." }
    }
  }
}
```

**Supported Content Types:**
- `ContentBlock::Text` - Text content (required)
- `ContentBlock::Image` - Image data
- `ContentBlock::Resource` - Embedded resources
- `ContentBlock::ResourceLink` - Links to resources

**Stop Reasons:**
- `end_turn` - Agent finished naturally
- `tool_use` - Agent called tools (internal, continues loop)
- `max_tokens` - Hit token limit
- `stop_sequence` - Hit stop sequence
- `cancelled` - User cancelled via `session/cancel`
- `error` - Error occurred

**Aptove Implementation Notes:**
- Implements full agentic loop with tool execution
- Streams all updates (text chunks, tool calls, thoughts)
- Persists user and assistant messages to `session.md`
- Tool calls and results are ephemeral (not persisted)

**Files:**
- Implementation: `agent-cli/src/main.rs` (`handle_session_prompt`)
- Agent loop: `agent-core/src/agent_loop.rs` (`run_agent_loop`)
- Persistence: `agent-core/src/persistence.rs`

---

### 3. `session/cancel` ❌

Cancels ongoing operations for a session.

**Notification:**
```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "abc-123"
  }
}
```

**Expected Behavior:**
- Agent cancels ongoing LLM calls, tool executions
- Agent responds to original `session/prompt` with `stopReason: "cancelled"`

**Status:** Not yet implemented in Aptove agent.

**TODO:**
- Add cancellation token propagation
- Handle cancellation in agent loop
- Respond with cancelled stop reason

---

### 4. `session/update` ✅

Notification sent from agent to client with streaming updates.

**Notification Types:**

**Agent Message Chunk:**
```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "abc-123",
    "update": {
      "type": "agentMessageChunk",
      "content": { "type": "text", "text": "partial response..." }
    }
  }
}
```

**Agent Thought Chunk:**
```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "abc-123",
    "update": {
      "type": "agentThoughtChunk",
      "content": { "type": "text", "text": "thinking about..." }
    }
  }
}
```

**Tool Call:**
```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "abc-123",
    "update": {
      "type": "toolCall",
      "toolCallId": "tc_123",
      "title": "Execute Bash Command",
      "status": "inProgress",
      "rawInput": { "command": "ls -la" }
    }
  }
}
```

**Tool Call Update:**
```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "abc-123",
    "update": {
      "type": "toolCallUpdate",
      "toolCallId": "tc_123",
      "status": "completed",
      "content": [
        { "type": "text", "text": "file1.txt\nfile2.txt" }
      ]
    }
  }
}
```

**Aptove Implementation Notes:**
- All streaming updates are sent via `session/update`
- Supports text deltas, tool call progress, thoughts
- Transport layer handles JSON-RPC serialization

**Files:**
- Emission: `agent-core/src/agent_loop.rs` (`emit_stream_event`)

---

### 5. `session/load` ❌

Loads an existing session with full message history.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session/load",
  "params": {
    "sessionId": "abc-123",
    "cwd": "/path/to/directory",
    "mcpServers": []
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "modes": { /* optional */ },
    "config": { /* optional */ }
  }
}
```

**Difference from `session/new`:**
- `session/load`: Loads session WITH message history replay
- `session/new` with `_meta.sessionId`: Reuses workspace but CLEARS conversation

**Status:** Not implemented in Aptove agent.

**Rationale:**
- Aptove uses `session/new` with `_meta.sessionId` for mobile app persistence
- Conversation is intentionally cleared on app restart for fresh context
- Workspace settings are preserved (config, MCP state)

**Future:** Could implement if users want to resume exact conversation state.

---

### 6. `session/resume` ❌

Resumes a session without replaying message history (lighter than `session/load`).

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session/resume",
  "params": {
    "sessionId": "abc-123",
    "cwd": "/path/to/directory",
    "mcpServers": []
  }
}
```

**Status:** Not implemented. Similar to current `session/new` with `_meta.sessionId` behavior.

---

### 7. `session/list` ❌

Lists all available sessions with pagination support.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "session/list",
  "params": {
    "cwd": "/optional/filter/by/directory",
    "cursor": "optional-pagination-cursor"
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "sessions": [
      {
        "sessionId": "abc-123",
        "name": "Project discussion",
        "created": "2026-02-16T10:30:00Z",
        "lastAccessed": "2026-02-16T12:45:00Z"
      }
    ],
    "nextCursor": "optional-next-page-token"
  }
}
```

**Status:** Not implemented.

**Workaround:** Use `aptove workspace list` CLI command.

---

### 8. `session/fork` ❌

Creates a copy of an existing session with a new session ID.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "session/fork",
  "params": {
    "sessionId": "abc-123",
    "cwd": "/path/to/directory",
    "mcpServers": []
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "sessionId": "new-forked-uuid"
  }
}
```

**Status:** Not implemented.

**Use Case:** Explore alternative conversation branches from a checkpoint.

---

### 9. `session/set_mode` ❌

Sets the session mode (e.g., "chat", "code", "plan").

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "session/set_mode",
  "params": {
    "sessionId": "abc-123",
    "modeId": "code"
  }
}
```

**Status:** Not implemented. Modes are defined in the protocol but not used by Aptove yet.

**Future:** Could enable different agent behaviors per mode (code generation, planning, chat).

---

### 10. `session/set_config_option` ❌

Sets a session-specific configuration option.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "session/set_config_option",
  "params": {
    "sessionId": "abc-123",
    "configId": "streaming",
    "value": "enabled"
  }
}
```

**Status:** Not implemented. Configuration is currently global or workspace-scoped (via `workspace.toml`).

---

### 11. `session/set_model` ❌

Changes the LLM model for a session.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 9,
  "method": "session/set_model",
  "params": {
    "sessionId": "abc-123",
    "modelId": "claude-sonnet-4-5"
  }
}
```

**Status:** Not implemented. Model is set at session creation via workspace config.

**Workaround:** Create a new session with a different workspace config.

---

## Other ACP Methods

### `initialize` (Required)

Handshake to establish connection and exchange capabilities.

**Request:**
```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientInfo": {
      "name": "Aptove iOS",
      "version": "1.0.0"
    },
    "capabilities": {
      "terminal": true
    }
  }
}
```

**Response:**
```json
{
  "jsonrpc": "2.0",
  "id": 0,
  "result": {
    "protocolVersion": 1,
    "agentInfo": {
      "name": "Aptove Agent",
      "version": "0.1.0"
    },
    "agentCapabilities": {
      "loadSession": false,
      "sessionCapabilities": {},
      "promptCapabilities": {}
    }
  }
}
```

**Status:** ✅ Implemented

---

## Client Operations (Agent→Client Requests)

These are requests the **agent** makes to the **client**:

| Method | Status | Description |
|--------|--------|-------------|
| `session/request_permission` | ✅ Implemented | Request user approval for tool execution |
| `terminal/create` | ✅ Implemented | Create a terminal session |
| `terminal/output` | ✅ Implemented | Get terminal output |
| `terminal/release` | ✅ Implemented | Release terminal session |
| `fs/read_text_file` | ❌ Stub | Read a text file (client filesystem) |
| `fs/write_text_file` | ❌ Stub | Write a text file (client filesystem) |

---

## Summary

### Implementation Coverage

| Category | Implemented | Total | Coverage |
|----------|-------------|-------|----------|
| Core session methods | 3/4 | 4 | 75% |
| Optional session methods | 0/7 | 7 | 0% |
| Client operations | 4/6 | 6 | 67% |
| **Overall** | **7/17** | **17** | **41%** |

### Priority Roadmap

**High Priority:**
1. ✅ `session/new` - Implemented with mobile app persistence
2. ✅ `session/prompt` - Full agentic loop with streaming
3. ❌ `session/cancel` - Cancel ongoing operations

**Medium Priority:**
4. ❌ `session/load` - Load session with history
5. ❌ `session/list` - List available sessions
6. ❌ `session/fork` - Fork conversation branches

**Low Priority:**
7. ❌ `session/resume` - Already covered by `session/new` with `_meta`
8. ❌ `session/set_mode` - Not needed yet
9. ❌ `session/set_config_option` - Workspace config covers this
10. ❌ `session/set_model` - Workspace config covers this

---

## Related Documentation

- [session.md](./session.md) - Session persistence and management
- [workspace.md](./workspace.md) - Workspace architecture
- [ACP Protocol Spec](https://agentclientprotocol.com/) - Official protocol documentation
