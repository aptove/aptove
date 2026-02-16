# Workspaces

Aptove uses **workspaces** to isolate state for each user or device. A workspace is a UUID-identified folder that holds everything for one user — configuration overrides, a persistent conversation, MCP server state, and placeholder directories for future skills and memory.

## Concepts

| Concept | Description |
|---------|-------------|
| **Workspace** | A UUID-named folder under `<data_dir>/Aptove/workspaces/<uuid>/` containing all user-scoped state |
| **Binding** | A mapping from a device identifier to a workspace UUID, stored in `bindings.toml` |
| **Default workspace** | A well-known workspace at `workspaces/default/` used in CLI single-user mode |

> **Note**: For session management (conversation persistence), see [session.md](./session.md)

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
    │   ├── session.md             # Persistent conversation (see session.md)
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
- The workspace folder with a UUID v4 name (or provided session ID from mobile apps)
- `workspace.toml` — metadata (uuid, name, created_at, last_accessed, provider)
- An empty `session.md`
- Subdirectories: `mcp/`, `skills/`, `memory/`

### Resolution (Device Binding)

When a device connects through the bridge:

1. The `session/new` ACP request may include:
   - `device_id` parameter (legacy, deprecated)
   - `_meta.sessionId` (preferred for mobile apps)
2. `WorkspaceManager::resolve(identifier)` checks `BindingStore` for an existing mapping
3. If a binding exists → loads that workspace
4. If no binding exists → creates a new workspace, binds the device, returns the new workspace

```
Device connects → BindingStore::resolve(identifier)
                     ├── Found → WorkspaceStore::load(uuid) → load workspace
                     └── Not found → WorkspaceStore::create(new_uuid)
                                   → BindingStore::bind(identifier, new_uuid)
```

**Mobile apps** pass a persistent `sessionId` in `_meta` to maintain the same workspace across app restarts. See [session.md](./session.md) for details.

### Deletion

`aptove workspace delete <uuid>` performs:
1. Remove all device bindings pointing to the workspace (`BindingStore::unbind_workspace`)
2. Clear the session data (`SessionStore::clear`)
3. Delete the workspace folder (`WorkspaceStore::delete`)

### Garbage Collection

`aptove workspace gc --max-age 90` removes workspaces not accessed within N days. The `default` workspace is never garbage-collected.

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

**Mobile apps**: Session IDs from `_meta.sessionId` are used as workspace identifiers, creating a direct 1:1 mapping between session and workspace.

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
| `/clear` | Reset session (see [session.md](./session.md)) |
| `/context` | Show current context window message count |
| `/help` | Show all available commands |
| `/quit` | Exit chat mode |

## Pluggable Storage Backends

All storage operations are defined as async traits in `agent-core`:

| Trait | Purpose | Methods |
|-------|---------|---------|
| `WorkspaceStore` | Workspace CRUD + GC | `create`, `load`, `list`, `delete`, `update_accessed`, `gc`, `load_config` |
| `SessionStore` | Session persistence | `read`, `append`, `write`, `clear` (see [session.md](./session.md)) |
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
  session/new → resolve workspace → load session → create in-memory session
  session/prompt → run agent loop → persist messages → return response
  session/cancel → cancel running loop
```

## Workspace vs. Session

| Aspect | Workspace | Session |
|--------|-----------|---------|
| **Scope** | All user state for a device/user | Single persistent conversation |
| **Storage** | Folder with multiple files | Single `session.md` file |
| **Lifecycle** | Created once, persists indefinitely | Can be cleared, regenerated |
| **Identifier** | UUID (or session ID from mobile) | Stored within workspace folder |
| **Contents** | Config, session, MCP state, skills, memory | Conversation messages only |

> **Key Point**: Workspaces are containers. Sessions are the conversation inside them. One workspace = one session.

## Best Practices

1. **Use session IDs as workspace identifiers** (mobile apps) — Maintains consistency across restarts
2. **Implement custom stores carefully** — All three traits must be thread-safe and durable
3. **Workspace config is optional** — Don't create it unless overriding global settings
4. **GC regularly** — Prevents unbounded workspace accumulation
5. **Bindings are sacred** — Never manually edit `bindings.toml`, use the API
6. **Per-workspace configs** — Use for different LLM providers or models per project/user
7. **Test corruption handling** — Ensure graceful degradation when files are malformed

## Related Documentation

- [session.md](./session.md) — Session management and persistence
- [config.md](./config.md) — Configuration format and options (if exists)
- [mcp.md](./mcp.md) — MCP server integration (if exists)
