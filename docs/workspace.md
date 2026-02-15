# Workspaces & Sessions

Aptove uses **workspaces** to isolate state for each user or device. A workspace is a UUID-identified folder that holds everything for one user — configuration overrides, a persistent conversation (`session.md`), MCP server state, and placeholder directories for future skills and memory.

## Concepts

| Concept | Description |
|---------|-------------|
| **Workspace** | A UUID-named folder under `<data_dir>/Aptove/workspaces/<uuid>/` containing all user-scoped state |
| **Session** | A single persistent conversation stored as `session.md` inside a workspace. One workspace = one session |
| **Binding** | A mapping from a device identifier to a workspace UUID, stored in `bindings.toml` |
| **Default workspace** | A well-known workspace at `workspaces/default/` used in CLI single-user mode |

## Data Directory Layout

All workspace data lives under the platform-specific data directory:

| OS | Path |
|---|---|
| macOS | `~/Library/Application Support/Aptove/` |
| Linux | `~/.local/share/Aptove/` |
| Windows | `%LOCALAPPDATA%\Aptove\` |

```
<data_dir>/Aptove/
├── config.toml                    # Global configuration
├── bindings.toml                  # Device → workspace mappings
└── workspaces/
    ├── default/                   # Default workspace (CLI mode)
    │   ├── workspace.toml         # Workspace metadata
    │   ├── config.toml            # Per-workspace config overrides (optional)
    │   ├── session.md             # Persistent conversation
    │   ├── mcp/                   # MCP server state/logs
    │   ├── skills/                # Skill definitions (future)
    │   └── memory/                # Long-term memory (future)
    ├── 550e8400-e29b-41d4-.../    # Device-bound workspace
    │   └── ...
    └── 6ba7b810-9dad-11d1-.../    # Another workspace
        └── ...
```

## Workspace Lifecycle

### Creation

A workspace is created when:
- A new device connects via bridge and has no existing binding
- A user runs `aptove workspace create`
- The agent starts in CLI `chat` mode and no default workspace exists

On creation, the filesystem backend (`FsWorkspaceStore`) generates:
- The workspace folder with a UUID v4 name
- `workspace.toml` — metadata (uuid, name, created_at, last_accessed, provider)
- An empty `session.md`
- Subdirectories: `mcp/`, `skills/`, `memory/`

### Resolution (Device Binding)

When a device connects through the bridge:

1. The `session/new` ACP request includes a `device_id` parameter
2. `WorkspaceManager::resolve(device_id)` checks `BindingStore` for an existing mapping
3. If a binding exists → loads that workspace and resumes the session
4. If no binding exists → creates a new workspace, binds the device, returns the new workspace

```
Device connects → BindingStore::resolve(device_id)
                     ├── Found → WorkspaceStore::load(uuid) → resume session.md
                     └── Not found → WorkspaceStore::create(new_uuid)
                                   → BindingStore::bind(device_id, new_uuid)
```

### Deletion

`aptove workspace delete <uuid>` performs:
1. Remove all device bindings pointing to the workspace (`BindingStore::unbind_workspace`)
2. Clear the session data (`SessionStore::clear`)
3. Delete the workspace folder (`WorkspaceStore::delete`)

### Garbage Collection

`aptove workspace gc --max-age 90` removes workspaces not accessed within N days. The `default` workspace is never garbage-collected.

## Session Persistence

### `session.md` Format

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

**Rules:**
- The frontmatter block (`---`) contains provider, model, and creation timestamp
- Each message starts with a `## Role` header (`## User`, `## Assistant`, `## System`)
- Only text messages are persisted — tool calls and tool results are ephemeral
- The file is append-only during normal operation (each turn appends a new `## Role` block)

### Session Lifecycle

| Event | Action |
|-------|--------|
| Workspace created | Empty `session.md` written |
| Device connects | `session.md` is parsed, messages restored into the context window |
| User sends prompt | User message appended to `session.md` before LLM call |
| Agent responds | Assistant message appended to `session.md` after loop completes |
| User sends `/clear` | `session.md` reset to empty, in-memory context cleared |
| Corrupt `session.md` | Warning logged, agent starts with empty context |

### What Gets Persisted vs. What Doesn't

| Persisted | Not Persisted |
|-----------|---------------|
| User text messages | Tool call requests |
| Assistant text responses | Tool call results |
| System prompt (if custom) | Intermediate tool loop messages |
| Frontmatter metadata | Token usage stats |

## Configuration Layering

Configuration resolves in three layers (highest priority wins):

1. **Global config** — `<data_dir>/Aptove/config.toml`
2. **Workspace config** — `<data_dir>/Aptove/workspaces/<uuid>/config.toml` (overrides specific fields)
3. **Environment variables** — `ANTHROPIC_API_KEY`, `GOOGLE_API_KEY`, `OPENAI_API_KEY`

When a `session/new` request resolves a workspace, the handler:
1. Loads the global config
2. Checks for a per-workspace `config.toml`
3. Merges them recursively (workspace fields override global, missing fields inherit)
4. Uses the merged config for provider/model selection

**Example:** Global config sets `provider = "claude"`. Workspace B's `config.toml` sets `provider = "gemini"`. Workspace B uses Gemini; all others use Claude.

```toml
# workspaces/<uuid>/config.toml — only override what's different
provider = "gemini"

[providers.gemini]
model = "gemini-2.5-flash"
```

## Device Bindings

Bindings are stored in `<data_dir>/Aptove/bindings.toml`:

```toml
[bindings]
"device-id-abc123" = "550e8400-e29b-41d4-a716-446655440000"
"device-id-def456" = "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
```

The `FsBindingStore` keeps an in-memory cache and flushes to disk on every mutation for durability.

## CLI Commands

```bash
# List all workspaces
aptove workspace list

# Create a new workspace
aptove workspace create --name "project-x"

# Show workspace details
aptove workspace show <uuid>

# Delete a workspace
aptove workspace delete <uuid>

# Garbage-collect stale workspaces (default: 90 days)
aptove workspace gc --max-age 90
```

In **chat mode**, slash commands interact with the current workspace:

| Command | Description |
|---------|-------------|
| `/workspace` | Show current workspace name, UUID, provider, model, message count |
| `/clear` | Reset `session.md` and clear the in-memory context window |
| `/context` | Show current context window message count |
| `/help` | Show all available commands |
| `/quit` | Exit chat mode |

## Pluggable Storage Backends

All storage operations are defined as async traits in `agent-core`:

| Trait | Purpose | Methods |
|-------|---------|---------|
| `WorkspaceStore` | Workspace CRUD + GC | `create`, `load`, `list`, `delete`, `update_accessed`, `gc`, `load_config` |
| `SessionStore` | Session persistence | `read`, `append`, `write`, `clear` |
| `BindingStore` | Device → workspace mapping | `resolve`, `bind`, `unbind`, `unbind_workspace` |

The default filesystem implementation ships as the `agent-storage-fs` crate. Third parties can implement alternative backends (e.g., SQLite, Postgres, S3) by implementing these traits and passing them to `AgentBuilder`:

```rust
let agent = AgentBuilder::new(config)
    .with_llm(Arc::new(ClaudeProvider::new(&api_key, &model, None)))
    .with_workspace_store(Arc::new(MyCustomWorkspaceStore::new()?))
    .with_session_store(Arc::new(MyCustomSessionStore::new()?))
    .with_binding_store(Arc::new(MyCustomBindingStore::new()?))
    .build()?;
```

A single struct may implement all three traits, or different structs from different crates can fill different slots.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│  AgentBuilder                                           │
│  ├── .with_llm(provider)                                │
│  ├── .with_workspace_store(store)                       │
│  ├── .with_session_store(store)                         │
│  ├── .with_binding_store(store)                         │
│  └── .build() → AgentRuntime                            │
│         ├── WorkspaceManager                            │
│         │     ├── dyn WorkspaceStore                    │
│         │     ├── dyn SessionStore                      │
│         │     └── dyn BindingStore                      │
│         ├── LLM Providers (Claude, Gemini, OpenAI)      │
│         └── Plugins (MCP Bridge, etc.)                  │
└─────────────────────────────────────────────────────────┘

ACP Request Flow:
  initialize → agent info + capabilities
  session/new → resolve workspace → load session.md → create in-memory session
  session/prompt → run agent loop → persist messages → return response
  session/cancel → cancel running loop
```
