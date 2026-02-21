use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use uuid::Uuid;

use agent_core::config::AgentConfig;
use agent_core::trigger::{TriggerDefinition, TriggerEvent, TriggerStore};
use agent_scheduler::Scheduler;
use agent_storage_fs::{FsSchedulerStore, FsTriggerStore};

use crate::commands::TriggersAction;
use crate::runtime::build_runtime;

pub async fn run_triggers_command(action: TriggersAction, config: AgentConfig) -> Result<()> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    let store = Arc::new(
        FsTriggerStore::new(&data_dir).context("failed to initialize trigger store")?,
    );

    let runtime = build_runtime(config).await?;

    macro_rules! resolve_ws {
        ($opt:expr) => {
            if let Some(id) = $opt {
                id
            } else {
                runtime.workspace_manager().default().await?.uuid
            }
        };
    }

    match action {
        TriggersAction::List { workspace } => {
            let ws_id = resolve_ws!(workspace);
            let triggers = store.list_triggers(&ws_id).await?;

            if triggers.is_empty() {
                println!("No webhook triggers in workspace {}.", ws_id);
            } else {
                println!(
                    "{:<38} {:<24} {:<10} {}",
                    "ID", "NAME", "STATUS", "LAST EVENT"
                );
                println!("{}", "-".repeat(90));
                for t in &triggers {
                    let status = if t.enabled { "active" } else { "disabled" };
                    let last_event = t
                        .last_event_at
                        .map(|ts| ts.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<38} {:<24} {:<10} {}",
                        t.id,
                        &t.name[..t.name.len().min(23)],
                        status,
                        last_event,
                    );
                }
            }
        }

        TriggersAction::Create {
            name,
            prompt,
            rate_limit,
            hmac_secret,
            content_types,
            workspace,
        } => {
            let ws_id = resolve_ws!(workspace);
            let accepted_content_types: Vec<String> = content_types
                .unwrap_or_default()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();

            let now = chrono::Utc::now();
            let trigger = TriggerDefinition {
                id: Uuid::new_v4().to_string(),
                name: name.clone(),
                prompt_template: prompt,
                token: agent_core::trigger::generate_token(),
                enabled: true,
                notify: true,
                max_history: 20,
                rate_limit_per_minute: rate_limit,
                hmac_secret,
                accepted_content_types,
                created_at: now,
                updated_at: now,
                last_event_at: None,
            };

            let created = store.create_trigger(&ws_id, &trigger).await?;
            println!("✅ Created trigger: {}", created.id);
            println!("   Name:      {}", created.name);
            println!("   Token:     {}", created.token);
            println!("   Workspace: {}", ws_id);
            println!("\nWebhook URL (when served): /webhook/{}", created.token);
        }

        TriggersAction::Show { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;

            println!("Trigger: {}", t.id);
            println!("  Name:          {}", t.name);
            println!("  Enabled:       {}", t.enabled);
            println!("  Notify:        {}", t.notify);
            println!("  Rate limit:    {}/min", t.rate_limit_per_minute);
            println!("  Max history:   {}", t.max_history);
            if let Some(ref s) = t.hmac_secret {
                println!("  HMAC secret:   {} (set)", s.chars().take(4).collect::<String>());
            }
            if !t.accepted_content_types.is_empty() {
                println!("  Content types: {}", t.accepted_content_types.join(", "));
            }
            println!("  Created:       {}", t.created_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(ts) = t.last_event_at {
                println!("  Last event:    {}", ts.format("%Y-%m-%d %H:%M:%S"));
            }
            println!("  Token:         {}", t.token);
            println!("\nPrompt template:\n{}", t.prompt_template);

            let runs = store.list_runs(&ws_id, &trigger_id, 1).await.unwrap_or_default();
            if let Some(run) = runs.first() {
                println!("\nLatest run ({}):", run.timestamp.format("%Y-%m-%d %H:%M:%S"));
                println!("  Status:  {}", run.status);
                println!("  Payload: {}", run.payload_preview);
                if !run.output.is_empty() {
                    println!("\n{}", run.output);
                }
                if let Some(ref err) = run.error_message {
                    println!("  Error: {}", err);
                }
            }
        }

        TriggersAction::Test { trigger_id, payload, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let trigger = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;

            println!("Testing trigger '{}' ({})...", trigger.name, trigger.id);

            let provider = runtime.active_provider()
                .context("no active LLM provider")?;

            let event = TriggerEvent {
                trigger_id: trigger.id.clone(),
                workspace_id: ws_id.clone(),
                payload: payload.clone(),
                content_type: "application/json".to_string(),
                headers: HashMap::new(),
                received_at: chrono::Utc::now(),
            };

            let sched_store = Arc::new(
                FsSchedulerStore::new(&data_dir).context("failed to initialize scheduler store")?
            );
            let ws_store = runtime.workspace_manager().workspace_store().clone();
            let scheduler = Scheduler::new(sched_store, ws_store, provider);
            let run = scheduler.execute_trigger(&trigger, &event).await;

            store.write_run(&ws_id, &trigger.id, &run).await.ok();

            println!("\nStatus: {} ({} ms)", run.status, run.duration_ms);
            if !run.output.is_empty() {
                println!("\n{}", run.output);
            }
            if let Some(ref err) = run.error_message {
                eprintln!("Error: {}", err);
            }
        }

        TriggersAction::Enable { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            t.enabled = true;
            t.updated_at = chrono::Utc::now();
            store.update_trigger(&ws_id, &t).await?;
            println!("✅ Trigger '{}' enabled.", t.name);
        }

        TriggersAction::Disable { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            t.enabled = false;
            t.updated_at = chrono::Utc::now();
            store.update_trigger(&ws_id, &t).await?;
            println!("Trigger '{}' disabled.", t.name);
        }

        TriggersAction::Delete { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.get_trigger(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            store.delete_trigger(&ws_id, &trigger_id).await?;
            println!("🗑  Deleted trigger '{}' ({}).", t.name, trigger_id);
        }

        TriggersAction::RegenerateToken { trigger_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let t = store.regenerate_token(&ws_id, &trigger_id).await
                .with_context(|| format!("trigger '{}' not found", trigger_id))?;
            println!("✅ Token regenerated for trigger '{}'.", t.name);
            println!("   New token: {}", t.token);
            println!("\nNew webhook URL (when served): /webhook/{}", t.token);
        }
    }

    Ok(())
}
