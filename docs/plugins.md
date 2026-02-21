# Plugin Development Guide

This guide covers everything a plugin vendor needs to build, test, and distribute
a plugin for the Aptove agent.

## Overview

Plugins extend the agent at runtime through a set of **lifecycle hooks**. A plugin
is a Rust crate that implements the `Plugin` trait from `agent-core`. When the
agent runs a prompt, it calls each registered plugin's hooks in registration order,
giving plugins a chance to inspect and mutate tool calls, output chunks, session
state, and the context window.

Plugins are **not** responsible for executing tools or calling the LLM — that is
handled by the agent loop. Plugins observe and can transform the data that flows
through the loop.

### What plugins can do

| Hook | What you can do |
|---|---|
| `on_init` | Read config, open file handles, connect to external services |
| `on_session_start` | Inspect or annotate the new session, inject initial context |
| `on_before_tool_call` | Inspect or mutate tool arguments; block a call by returning an error |
| `on_after_tool_call` | Post-process tool output; record undo state |
| `on_output_chunk` | Reformat or annotate each text chunk flowing to the user |
| `on_context_compact` | Replace dropped messages with a custom summary |
| `on_shutdown` | Flush state, close connections |

---

## Quick Start

### 1. Create a new crate

```
cargo new --lib agent-plugin-myplugin
```

### 2. Add dependencies to `Cargo.toml`

```toml
[package]
name = "agent-plugin-myplugin"
version = "0.1.0"
edition = "2021"

[dependencies]
agent-core = { version = "0.1" }   # or path = "../agent-core" in a workspace
anyhow = "1"
async-trait = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
```

### 3. Implement the `Plugin` trait

```rust
use agent_core::plugin::{OutputChunk, Plugin};
use agent_core::types::{ToolCallRequest, ToolCallResult};
use agent_core::session::Session;
use anyhow::Result;
use async_trait::async_trait;

pub struct MyPlugin {
    config: MyPluginConfig,
}

impl MyPlugin {
    pub fn new() -> Self {
        Self { config: MyPluginConfig::default() }
    }
}

#[async_trait]
impl Plugin for MyPlugin {
    fn name(&self) -> &str {
        "my-plugin"
    }

    // All hook methods are optional — only override the ones you need.
}
```

### 4. Register with the agent

In your binary or integration, pass the plugin to `AgentBuilder`:

```rust
use agent_core::AgentBuilder;

let runtime = AgentBuilder::new(config)
    .with_llm(provider)
    .with_workspace_store(ws)
    .with_session_store(ss)
    .with_binding_store(bs)
    .with_plugin(Box::new(MyPlugin::new()))   // <-- register here
    .build()?;
```

---

## The `Plugin` Trait

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn name(&self) -> &str;

    async fn on_init(&mut self, config: &serde_json::Value) -> Result<()>;
    async fn on_shutdown(&self) -> Result<()>;

    async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()>;
    async fn on_after_tool_call(&self, call: &ToolCallRequest, result: &mut ToolCallResult) -> Result<()>;
    async fn on_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()>;
    async fn on_context_compact(&self, messages: &[Message], target_tokens: usize) -> Result<Option<Vec<Message>>>;
    async fn on_session_start(&self, session: &mut Session) -> Result<()>;
}
```

All hook methods have default no-op implementations; override only what you need.

### `name() -> &str`

Returns a stable, unique identifier for your plugin. Used in log messages and error
reporting. Use lowercase kebab-case, e.g. `"git-integration"`.

### `on_init(&mut self, config: &serde_json::Value)`

Called once at agent startup, before any sessions are created. The `config`
argument is the full serialised `AgentConfig` as a JSON value. Read your plugin's
config section from it:

```rust
async fn on_init(&mut self, config: &serde_json::Value) -> Result<()> {
    if let Some(section) = config.get("plugins").and_then(|p| p.get("my-plugin")) {
        self.config = serde_json::from_value(section.clone())?;
    }
    Ok(())
}
```

`on_init` is the only hook that receives `&mut self`, so it is the right place to
set up any mutable fields. Subsequent hooks receive `&self`; use `Arc<Mutex<T>>`
or `Arc<RwLock<T>>` for state that must be mutated across hook calls.

### `on_shutdown(&self)`

Called on graceful shutdown. Flush buffers, close file handles, send final
telemetry, etc. Errors are logged but do not abort the shutdown sequence.

### `on_session_start(&self, session: &mut Session)`

Called after a new `Session` is created, before the first prompt is processed.
The session is mutable — you can record its ID for later correlation or inject
initial metadata, but avoid adding user-visible messages at this stage.

```rust
async fn on_session_start(&self, session: &mut Session) -> Result<()> {
    tracing::info!(session_id = %session.id, "session started");
    Ok(())
}
```

### `on_before_tool_call(&self, call: &mut ToolCallRequest)`

Called before each tool call is dispatched. You can:

- **Inspect** the tool name and arguments.
- **Mutate** the arguments (e.g. canonicalise a file path).
- **Block** the call by returning an error (`anyhow::bail!`).

```rust
async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()> {
    if call.name == "write_file" {
        let path = call.arguments["path"].as_str().unwrap_or("");
        tracing::info!(tool = %call.name, path, "intercepted write_file");
    }
    Ok(())
}
```

`ToolCallRequest`:

| Field | Type | Description |
|---|---|---|
| `id` | `String` | Unique call ID assigned by the LLM provider |
| `name` | `String` | Tool name (e.g. `"write_file"`, `"bash"`) |
| `arguments` | `serde_json::Value` | JSON arguments; mutate in-place |

### `on_after_tool_call(&self, call: &ToolCallRequest, result: &mut ToolCallResult)`

Called after a tool returns its result. You can post-process or annotate the
output, append metadata, or stage side effects (e.g. `git add` after a file
write).

```rust
async fn on_after_tool_call(
    &self,
    call: &ToolCallRequest,
    result: &mut ToolCallResult,
) -> Result<()> {
    if result.is_error {
        tracing::warn!(tool = %call.name, "tool call failed");
    }
    Ok(())
}
```

`ToolCallResult`:

| Field | Type | Description |
|---|---|---|
| `tool_call_id` | `String` | Matches the `id` from `ToolCallRequest` |
| `content` | `String` | The tool's text output; mutate to post-process |
| `is_error` | `bool` | `true` if the tool reported a failure |

### `on_output_chunk(&self, chunk: &mut OutputChunk)`

Called for each text chunk flowing to the user during streaming. Plugins are
called in registration order; each plugin sees the chunk already mutated by
earlier plugins.

```rust
async fn on_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()> {
    // Wrap final chunks in a separator
    if chunk.is_final {
        chunk.text.push_str("\n---\n");
    }
    Ok(())
}
```

`OutputChunk`:

| Field | Type | Description |
|---|---|---|
| `text` | `String` | The text content; mutate in-place |
| `is_final` | `bool` | `true` for the last chunk in the current assistant turn |

### `on_context_compact(&self, messages: &[Message], target_tokens: usize)`

Called when the context window nears its token limit and is about to drop older
messages. Return `Some(replacement_messages)` to substitute your own summary for
the dropped messages. Return `None` to let the default truncation proceed.

Only the **first** plugin that returns `Some` wins; subsequent plugins are not
called.

```rust
async fn on_context_compact(
    &self,
    messages: &[Message],
    target_tokens: usize,
) -> Result<Option<Vec<Message>>> {
    let summary = summarise(messages);   // your summarisation logic
    Ok(Some(vec![Message {
        role: Role::Assistant,
        content: MessageContent::Text(summary),
    }]))
}
```

---

## Configuration

Plugins read their configuration from the agent's TOML config file. The
conventional location is a table named after your plugin under `[plugins]`:

```toml
[plugins.my-plugin]
enabled = true
some_option = "value"
```

Define a config struct and deserialise it in `on_init`:

```rust
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MyPluginConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub some_option: Option<String>,
}

fn default_enabled() -> bool { true }

impl Default for MyPluginConfig {
    fn default() -> Self {
        Self { enabled: true, some_option: None }
    }
}
```

---

## Interior Mutability

All plugin hooks except `on_init` receive `&self`, so state shared across hook
calls must use interior mutability. The standard pattern is `Arc<Mutex<T>>`:

```rust
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct MyPlugin {
    config: MyPluginConfig,
    state: Arc<Mutex<MyPluginState>>,
}

struct MyPluginState {
    call_count: usize,
}

#[async_trait]
impl Plugin for MyPlugin {
    fn name(&self) -> &str { "my-plugin" }

    async fn on_before_tool_call(&self, _call: &mut ToolCallRequest) -> Result<()> {
        let mut state = self.state.lock().await;
        state.call_count += 1;
        Ok(())
    }
}
```

Use `tokio::sync::Mutex` (not `std::sync::Mutex`) inside async hooks to avoid
blocking the executor.

---

## Hook Execution Order

When multiple plugins are registered, hooks run in **registration order**. For
hooks that mutate a shared value (`on_before_tool_call`, `on_after_tool_call`,
`on_output_chunk`), each plugin sees the value after it has been modified by all
earlier plugins.

For `on_context_compact`, execution stops at the first plugin that returns
`Some(...)`.

---

## Error Handling

- **`on_init`**: returning an error aborts agent startup.
- **`on_shutdown`**: errors are logged and ignored; shutdown continues.
- **`on_before_tool_call`**: returning an error prevents the tool call from
  executing and surfaces the error as a tool failure in the conversation.
- **`on_after_tool_call`, `on_output_chunk`**: errors are logged as warnings but
  do not abort the agent loop.
- **`on_context_compact`**: errors are logged; the default truncation proceeds.
- **`on_session_start`**: errors propagate to the session creation caller.

---

## Testing

Test your plugin in isolation by constructing it directly and calling hooks:

```rust
#[tokio::test]
async fn test_before_tool_call() {
    let plugin = MyPlugin::new();
    let mut call = ToolCallRequest {
        id: "tc1".into(),
        name: "write_file".into(),
        arguments: serde_json::json!({ "path": "/tmp/test.txt" }),
    };
    plugin.on_before_tool_call(&mut call).await.unwrap();
    // assert on mutations
}
```

For integration tests that involve the full agent loop, build an `AgentRuntime`
with mock stores and a mock LLM provider, then run a prompt and verify your
plugin's side effects.

---

## Real-World Examples

Two plugins ship with the agent and serve as reference implementations.

### `agent-plugin-git` — Git integration

Located at `plugins/agent-plugin-git/`.

- **`on_session_start`**: detects whether the working directory is a git repo;
  warns if there are uncommitted changes.
- **`on_before_tool_call`**: for file-write tool calls, warns if the target file
  has uncommitted changes.
- **`on_after_tool_call`**: after a successful file write, stages the file with
  `git add`.
- **`on_output_chunk`** (`is_final == true`): commits all files staged during
  the current agent turn with an auto-generated message.
- **`on_shutdown`**: flushes any remaining staged files.

Config section:

```toml
[plugins.git]
auto_commit = true
commit_message_prefix = "agent: "
warn_dirty = true
```

### `agent-plugin-diff` — Edit-engine plugin

Located at `plugins/agent-plugin-diff/`.

- **`on_before_tool_call`**: intercepts `write_file` calls and transparently
  applies structured edit formats that LLMs commonly produce (search-and-replace
  blocks, unified diffs, whole-file rewrites), so the downstream filesystem tool
  always receives the final file content.

No config section required; the plugin activates for any file-write tool call.

---

## Publishing Your Plugin

A plugin crate is a normal Rust library. Publish it to [crates.io](https://crates.io)
and users can add it to their `Cargo.toml` as a dependency. Document which config
section your plugin reads and which tool names it intercepts so users know what to
expect.

For plugins intended to be bundled with a custom agent binary, add the crate to the
workspace and register it in the binary's `build_runtime()` call.
