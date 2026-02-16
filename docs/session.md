# Session Management

Sessions in Aptove represent a persistent conversation between a user and the agent. Each workspace has one session stored as `session.md`.

## Session ID Persistence (Mobile Apps)

### Overview

Mobile apps (iOS and Android) maintain a persistent session ID that survives app restarts. This allows:
- **Workspace persistence**: Same workspace folder across app launches
- **Conversation clearing**: Users can reset chat without losing workspace settings
- **Cross-platform consistency**: Same behavior on iOS and Android

### How It Works

#### First Launch
```
Mobile App → session/new (no _meta)
           ↓
Agent → Generates UUID: "abc-123"
      → Creates workspace: /workspaces/abc-123/
      → Returns sessionId: "abc-123"
           ↓
Mobile App → Saves "abc-123" to local storage
             (UserDefaults/SharedPreferences)
```

#### App Restart
```
Mobile App → session/new with _meta: { sessionId: "abc-123" }
           ↓
Agent → Finds workspace: /workspaces/abc-123/
      → CLEARS session.md content
      → Reuses sessionId: "abc-123"
      → Returns sessionId: "abc-123"
           ↓
Mobile App → Fresh conversation, same workspace
```

#### Clear Session
```
Mobile App → clearSession()
           → Disconnect
           → Reconnect
           → session/new with _meta: { sessionId: "abc-123" }
           ↓
Agent → Clears session.md
      → Fresh conversation
      → Workspace settings retained
```

### Implementation

**Agent (Rust)**
- Extracts `sessionId` from `params._meta.sessionId`
- Uses it as workspace identifier (folder name)
- Clears `session.md` when reusing existing session
- Creates session with the provided ID

**iOS (Swift)**
- Stores session ID in UserDefaults
- Passes it in `_meta` field when calling `session/new`
- Scopes per agent: `aptove.sessionId.<agentId>`

**Android (Kotlin)**
- Stores session ID in SharedPreferences
- Passes it in `_meta` field when calling `session/new`
- Scopes globally: `acp_client_prefs.session_id`

## Session.md Format

Each workspace has exactly one session file stored as human-readable structured markdown:

```markdown
---
provider: claude
model: claude-sonnet-4-20250514
created_at: 2026-02-15T10:30:00Z
---

## User

What files are in the current directory?

## Assistant

I'll check the directory listing for you. Here are the files:
- src/
- Cargo.toml
- README.md

## User

Show me Cargo.toml

## Assistant

Here's the contents of Cargo.toml:
...
```

### Format Rules

- **Frontmatter**: YAML block with provider, model, created_at
- **Message headers**: `## Role` (User, Assistant, System)
- **Text only**: Tool calls and results are ephemeral (not persisted)
- **Append-only**: Each turn appends a new `## Role` block

## Session Lifecycle

| Event | Action |
|-------|--------|
| Workspace created | Empty `session.md` written |
| Device connects | `session.md` parsed, messages restored to context |
| User sends prompt | User message appended before LLM call |
| Agent responds | Assistant message appended after loop completes |
| User clears session | `session.md` reset to empty, context cleared |
| Corrupt `session.md` | Warning logged, agent starts with empty context |
| Mobile app restarts | `session.md` cleared (if sessionId in `_meta`) |

## What Gets Persisted

| Persisted | Not Persisted |
|-----------|---------------|
| User text messages | Tool call requests |
| Assistant text responses | Tool call results |
| System prompt (if custom) | Intermediate tool loop messages |
| Frontmatter metadata | Token usage stats |

**Rationale**: Only the essential conversation context is saved. Tool execution details are ephemeral since they can be regenerated if needed.

## Session vs. Workspace

| Concept | Description |
|---------|-------------|
| **Session** | A single persistent conversation stored as `session.md` inside a workspace |
| **Workspace** | A folder containing all user-scoped state (session, config, MCP state, etc.) |
| **Relationship** | One workspace = one session. Workspaces persist across sessions. |

## CLI Commands

In **chat mode**, slash commands interact with the current session:

| Command | Description |
|---------|-------------|
| `/clear` | Reset `session.md` and clear the in-memory context window |
| `/context` | Show current context window message count |
| `/workspace` | Show workspace info (includes session message count) |

## SessionStore Trait

Session persistence is defined by the `SessionStore` trait in `agent-core`:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Read the full session for a workspace
    async fn read(&self, workspace_id: &str) -> Result<SessionData>;

    /// Append a single message to the session
    async fn append(&self, workspace_id: &str, message: &Message) -> Result<()>;

    /// Write the full session (overwrites existing)
    async fn write(&self, workspace_id: &str, data: &SessionData) -> Result<()>;

    /// Clear the session (reset to empty frontmatter)
    async fn clear(&self, workspace_id: &str) -> Result<()>;
}
```

Default implementation: `FsSessionStore` in `agent-storage-fs` crate.

## Mobile App APIs

### iOS (Swift)

```swift
// Store session ID
private func saveSessionId(_ sessionId: String)

// Load session ID
private func getStoredSessionId() -> String?

// Clear conversation (keeps session ID)
func clearSession() async
```

### Android (Kotlin)

```kotlin
// Store session ID
private fun saveSessionId(sessionId: String)

// Load session ID
private fun getStoredSessionId(): String?

// Clear conversation (keeps session ID)
suspend fun clearSession(): Result<ClientSession>
```

## Best Practices

1. **Never delete session.md directly** — Use `SessionStore::clear()` to maintain frontmatter
2. **Respect workspace boundaries** — Session IDs should map 1:1 to workspaces
3. **Handle corruption gracefully** — Parse errors should not crash the agent
4. **Persist incrementally** — Append messages as they arrive, don't wait for full turn
5. **Mobile apps**: Store session ID persistently to maintain workspace across restarts
6. **Clear, don't recreate**: Use `clearSession()` to reset conversation without losing workspace

## Architecture

```
┌─────────────────────────────────────────────┐
│ Session Management Flow                     │
├─────────────────────────────────────────────┤
│                                             │
│  Mobile App                                 │
│    ├── Save sessionId to storage            │
│    └── Pass in _meta: { sessionId }         │
│              ↓                              │
│  session/new request                        │
│    ├── Extract _meta.sessionId              │
│    └── Use as workspace identifier          │
│              ↓                              │
│  WorkspaceManager                           │
│    ├── Resolve workspace by sessionId       │
│    └── Clear session.md if reusing          │
│              ↓                              │
│  SessionStore                               │
│    ├── clear(workspace_id)                  │
│    ├── read(workspace_id)                   │
│    ├── append(workspace_id, message)        │
│    └── write(workspace_id, data)            │
│              ↓                              │
│  session.md file                            │
│    └── Persistent conversation storage      │
│                                             │
└─────────────────────────────────────────────┘
```
