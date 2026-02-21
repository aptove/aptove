use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use agent_core::config::AgentConfig;
use agent_core::{AgentBuilder, AgentRuntime};
use agent_mcp_bridge::McpBridge;
use agent_provider_claude::ClaudeProvider;
use agent_provider_gemini::GeminiProvider;
use agent_provider_openai::OpenAiProvider;
use agent_storage_fs::{FsBindingStore, FsSchedulerStore, FsSessionStore, FsTriggerStore, FsWorkspaceStore};

pub async fn build_runtime(config: AgentConfig) -> Result<AgentRuntime> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data directory: {}", data_dir.display()))?;

    info!(data_dir = %data_dir.display(), "using data directory");

    let mut builder = AgentBuilder::new(config.clone());
    let mut registered_providers: Vec<String> = Vec::new();

    if let Some(api_key) = config.resolve_api_key("claude") {
        let model = config.model_for_provider("claude");
        let base_url = config
            .providers
            .claude
            .as_ref()
            .and_then(|p| p.base_url.as_deref());
        info!(model = %model, "registering Claude provider");
        builder = builder.with_llm(Arc::new(ClaudeProvider::new(&api_key, &model, base_url)));
        registered_providers.push("claude".to_string());
    }
    if let Some(api_key) = config.resolve_api_key("gemini") {
        let model = config.model_for_provider("gemini");
        info!(model = %model, "registering Gemini provider");
        builder = builder.with_llm(Arc::new(GeminiProvider::new(&api_key, &model)));
        registered_providers.push("gemini".to_string());
    }
    if let Some(api_key) = config.resolve_api_key("openai") {
        let model = config.model_for_provider("openai");
        let base_url = config
            .providers
            .openai
            .as_ref()
            .and_then(|p| p.base_url.as_deref());
        info!(model = %model, "registering OpenAI provider");
        builder = builder.with_llm(Arc::new(OpenAiProvider::new(&api_key, &model, base_url)));
        registered_providers.push("openai".to_string());
    }

    if registered_providers.is_empty() {
        anyhow::bail!(
            "no LLM providers could be registered — no API keys found.\n\
             Set one of these environment variables:\n\
             • ANTHROPIC_API_KEY  (for Claude)\n\
             • GOOGLE_API_KEY    (for Gemini)\n\
             • OPENAI_API_KEY    (for OpenAI)\n\
             Or add api_key under [providers.<name>] in config.toml.\n\
             Config location: {}",
            AgentConfig::default_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into())
        );
    }

    info!(providers = ?registered_providers, active = %config.provider, "LLM providers registered");

    if !registered_providers.contains(&config.provider) {
        anyhow::bail!(
            "active provider '{}' has no API key configured.\n\
             Registered providers: [{}].\n\
             Either set the API key for '{}' or change the active provider in config.toml.",
            config.provider,
            registered_providers.join(", "),
            config.provider,
        );
    }
    builder = builder.with_active_provider(&config.provider);

    let ws_store = Arc::new(FsWorkspaceStore::new(&data_dir)
        .context("failed to initialize workspace store")?);
    let session_store = Arc::new(FsSessionStore::new(&data_dir)
        .context("failed to initialize session store")?);
    let binding_store = Arc::new(FsBindingStore::new_sync(&data_dir)
        .context("failed to initialize binding store")?);
    let scheduler_store = Arc::new(FsSchedulerStore::new(&data_dir)
        .context("failed to initialize scheduler store")?);
    let trigger_store = Arc::new(FsTriggerStore::new(&data_dir)
        .context("failed to initialize trigger store")?);

    builder = builder
        .with_workspace_store(ws_store)
        .with_session_store(session_store)
        .with_binding_store(binding_store)
        .with_scheduler_store(scheduler_store)
        .with_trigger_store(trigger_store);

    builder.build()
        .context("failed to build agent runtime")
}

pub async fn setup_mcp_bridge(
    config: &AgentConfig,
) -> Result<Option<Arc<tokio::sync::Mutex<McpBridge>>>> {
    // Filter to only servers whose command actually exists on PATH.
    let available: Vec<_> = config
        .mcp_servers
        .iter()
        .filter(|s| which::which(&s.command).is_ok())
        .cloned()
        .collect();

    if available.is_empty() {
        return Ok(None);
    }

    let mut bridge = McpBridge::new();
    bridge.connect_all(&available).await?;
    Ok(Some(Arc::new(tokio::sync::Mutex::new(bridge))))
}
