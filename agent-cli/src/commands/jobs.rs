use std::sync::Arc;

use anyhow::{Context, Result};
use uuid::Uuid;

use agent_core::agent_loop::AgentLoopConfig;
use agent_core::config::AgentConfig;
use agent_core::scheduler::{JobDefinition, JobRun, JobStatus, SchedulerStore};
use agent_core::types::{Message, MessageContent, Role};
use agent_scheduler::validate_cron;
use agent_storage_fs::FsSchedulerStore;

use crate::commands::JobsAction;
use crate::runtime::build_runtime;

pub async fn run_jobs_command(action: JobsAction, config: AgentConfig) -> Result<()> {
    let data_dir = AgentConfig::data_dir()
        .context("failed to determine data directory")?;
    let store = Arc::new(
        FsSchedulerStore::new(&data_dir).context("failed to initialize scheduler store")?,
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
        JobsAction::List { workspace } => {
            let ws_id = resolve_ws!(workspace);
            let jobs = store.list_jobs(&ws_id).await?;

            if jobs.is_empty() {
                println!("No scheduled jobs in workspace {}.", ws_id);
            } else {
                println!(
                    "{:<38} {:<24} {:<16} {:<10} {}",
                    "ID", "NAME", "SCHEDULE", "STATUS", "LAST RUN"
                );
                println!("{}", "-".repeat(110));
                for job in &jobs {
                    let status = if !job.enabled {
                        "disabled".to_string()
                    } else if job.last_run_at.is_none() {
                        "pending".to_string()
                    } else {
                        "enabled".to_string()
                    };
                    let last_run = job
                        .last_run_at
                        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "never".to_string());
                    println!(
                        "{:<38} {:<24} {:<16} {:<10} {}",
                        job.id,
                        &job.name[..job.name.len().min(23)],
                        job.schedule,
                        status,
                        last_run,
                    );
                }
            }
        }

        JobsAction::Create {
            name,
            prompt,
            schedule,
            timezone,
            one_shot,
            workspace,
        } => {
            validate_cron(&schedule)
                .with_context(|| format!("invalid cron expression: {:?}", schedule))?;

            let ws_id = resolve_ws!(workspace);
            let now = chrono::Utc::now();
            let job = JobDefinition {
                id: Uuid::new_v4().to_string(),
                name: name.clone(),
                prompt,
                schedule: schedule.clone(),
                timezone,
                enabled: true,
                one_shot,
                notify: true,
                max_history: 7,
                created_at: now,
                updated_at: now,
                last_run_at: None,
            };

            let created = store.create_job(&ws_id, &job).await?;
            println!("✅ Created job: {}", created.id);
            println!("   Name:     {}", created.name);
            println!("   Schedule: {}", created.schedule);
            println!("   Workspace: {}", ws_id);
        }

        JobsAction::Show { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;

            println!("Job: {}", job.id);
            println!("  Name:      {}", job.name);
            println!("  Schedule:  {}", job.schedule);
            if let Some(tz) = &job.timezone {
                println!("  Timezone:  {}", tz);
            }
            println!("  Enabled:   {}", job.enabled);
            println!("  One-shot:  {}", job.one_shot);
            println!("  Notify:    {}", job.notify);
            println!("  Max runs:  {}", job.max_history);
            println!("  Created:   {}", job.created_at.format("%Y-%m-%d %H:%M:%S"));
            if let Some(t) = job.last_run_at {
                println!("  Last run:  {}", t.format("%Y-%m-%d %H:%M:%S"));
            }
            println!("\nPrompt:\n{}", job.prompt);

            let runs = store.list_runs(&ws_id, &job_id, 1).await.unwrap_or_default();
            if let Some(run) = runs.first() {
                println!("\nLatest run ({}):", run.timestamp.format("%Y-%m-%d %H:%M:%S"));
                println!("  Status: {}", run.status);
                if !run.output.is_empty() {
                    println!("\n{}", run.output);
                }
                if let Some(ref err) = run.error_message {
                    println!("  Error: {}", err);
                }
            }
        }

        JobsAction::Run { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;

            println!("Running job '{}' ({})...", job.name, job.id);

            let provider = runtime.active_provider()
                .context("no active LLM provider")?;

            let messages = vec![Message {
                role: Role::User,
                content: MessageContent::Text(job.prompt.clone()),
            }];
            let cancel = tokio_util::sync::CancellationToken::new();
            let loop_cfg = AgentLoopConfig { max_iterations: 25, ..AgentLoopConfig::default() };
            let start = std::time::Instant::now();
            let timestamp = chrono::Utc::now();
            let run_id = Uuid::new_v4().to_string();

            let run = match agent_core::agent_loop::run_agent_loop(
                provider,
                &messages,
                &[],
                None,
                &loop_cfg,
                cancel,
                None,
                &run_id,
                Some(&runtime),
            )
            .await
            {
                Ok(result) => {
                    let output = result
                        .new_messages
                        .iter()
                        .filter_map(|m| match &m.content {
                            MessageContent::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    JobRun {
                        job_id: job.id.clone(),
                        timestamp,
                        status: JobStatus::Success,
                        output,
                        duration_ms: start.elapsed().as_millis() as u64,
                        error_message: None,
                    }
                }
                Err(e) => {
                    eprintln!("❌ Job execution failed: {}", e);
                    JobRun {
                        job_id: job.id.clone(),
                        timestamp,
                        status: JobStatus::Error,
                        output: String::new(),
                        duration_ms: start.elapsed().as_millis() as u64,
                        error_message: Some(e.to_string()),
                    }
                }
            };

            store.write_run(&ws_id, &job.id, &run).await.ok();

            let mut updated_job = job.clone();
            updated_job.last_run_at = Some(run.timestamp);
            updated_job.updated_at = chrono::Utc::now();
            if job.one_shot && run.status == JobStatus::Success {
                updated_job.enabled = false;
            }
            store.update_job(&ws_id, &updated_job).await.ok();
            if job.max_history > 0 {
                store.prune_runs(&ws_id, &job.id, job.max_history as usize).await.ok();
            }

            println!("\nStatus: {} ({} ms)", run.status, run.duration_ms);
            if !run.output.is_empty() {
                println!("\n{}", run.output);
            }
            if let Some(ref err) = run.error_message {
                eprintln!("Error: {}", err);
            }
        }

        JobsAction::Enable { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            job.enabled = true;
            job.updated_at = chrono::Utc::now();
            store.update_job(&ws_id, &job).await?;
            println!("✅ Job '{}' enabled.", job.name);
        }

        JobsAction::Disable { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let mut job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            job.enabled = false;
            job.updated_at = chrono::Utc::now();
            store.update_job(&ws_id, &job).await?;
            println!("Job '{}' disabled.", job.name);
        }

        JobsAction::Delete { job_id, workspace } => {
            let ws_id = resolve_ws!(workspace);
            let job = store.get_job(&ws_id, &job_id).await
                .with_context(|| format!("job '{}' not found", job_id))?;
            store.delete_job(&ws_id, &job_id).await?;
            println!("🗑  Deleted job '{}' ({}).", job.name, job_id);
        }
    }

    Ok(())
}
