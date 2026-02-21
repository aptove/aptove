//! Aptove Scheduler Engine
//!
//! Provides a cron-based background scheduler that fires jobs defined in a
//! [`SchedulerStore`] on their configured schedules. Each workspace is
//! scanned independently; due jobs are executed by calling the configured
//! LLM provider.
//!
//! # Usage
//!
//! ```rust,ignore
//! use std::sync::Arc;
//! use agent_scheduler::Scheduler;
//!
//! let scheduler = Scheduler::new(scheduler_store, workspace_store, provider);
//! let handle = scheduler.start(); // spawns background tokio task
//! // …later…
//! handle.stop().await;
//! ```

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use cron::Schedule;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use agent_core::plugin::{LlmProvider, Message, MessageContent, Role};
use agent_core::scheduler::{JobDefinition, JobRun, JobStatus, SchedulerStore};
use agent_core::trigger::{render_template, TriggerDefinition, TriggerEvent, TriggerRun, TriggerStatus};
use agent_core::workspace::WorkspaceStore;
use agent_core::agent_loop::{AgentLoopConfig, run_agent_loop};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Background scheduler that fires jobs on their cron schedules and
/// executes webhook trigger events.
pub struct Scheduler {
    store: Arc<dyn SchedulerStore>,
    workspace_store: Arc<dyn WorkspaceStore>,
    provider: Arc<dyn LlmProvider>,
}

impl Scheduler {
    /// Create a new scheduler.
    ///
    /// - `store` — reads/writes job definitions and run history.
    /// - `workspace_store` — lists active workspaces to scan.
    /// - `provider` — the LLM provider used to execute job prompts.
    pub fn new(
        store: Arc<dyn SchedulerStore>,
        workspace_store: Arc<dyn WorkspaceStore>,
        provider: Arc<dyn LlmProvider>,
    ) -> Self {
        Self {
            store,
            workspace_store,
            provider,
        }
    }

    /// Execute a webhook trigger event immediately (called by the ACP handler).
    ///
    /// Renders the trigger's prompt template with the event payload, sends it
    /// to the LLM provider, and returns a `TriggerRun` recording the outcome.
    pub async fn execute_trigger(
        &self,
        trigger: &TriggerDefinition,
        event: &TriggerEvent,
    ) -> TriggerRun {
        let rendered_prompt = render_template(&trigger.prompt_template, event, &trigger.name);
        let run_result = execute_prompt(&self.provider, &rendered_prompt).await;

        let payload_preview: String = event.payload.chars().take(200).collect();

        match run_result {
            Ok((output, duration_ms)) => TriggerRun {
                trigger_id: trigger.id.clone(),
                timestamp: event.received_at,
                status: TriggerStatus::Success,
                payload_preview,
                output,
                duration_ms,
                error_message: None,
                source_ip: None,
            },
            Err((err_msg, duration_ms)) => TriggerRun {
                trigger_id: trigger.id.clone(),
                timestamp: event.received_at,
                status: TriggerStatus::Error,
                payload_preview,
                output: String::new(),
                duration_ms,
                error_message: Some(err_msg),
                source_ip: None,
            },
        }
    }

    /// Start the scheduler background task. Returns a `SchedulerHandle` that
    /// can be used to stop the loop.
    pub fn start(self) -> SchedulerHandle {
        let store = self.store;
        let workspace_store = self.workspace_store;
        let provider = self.provider;

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();

        let task = tokio::spawn(async move {
            info!("scheduler started (tick interval: 60s)");

            // Running-job lock to prevent duplicate execution
            let running: Arc<tokio::sync::Mutex<HashSet<String>>> =
                Arc::new(tokio::sync::Mutex::new(HashSet::new()));

            loop {
                // Wait 60 seconds, or stop if signalled
                tokio::select! {
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {}
                    _ = &mut stop_rx => {
                        info!("scheduler stop requested — shutting down");
                        break;
                    }
                }

                if let Err(e) = tick(&store, &workspace_store, &provider, &running).await {
                    error!(err = %e, "scheduler tick error");
                }
            }

            info!("scheduler stopped");
        });

        SchedulerHandle {
            task,
            stop_tx: Some(stop_tx),
        }
    }
}

/// Handle to the running scheduler background task.
pub struct SchedulerHandle {
    task: tokio::task::JoinHandle<()>,
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl SchedulerHandle {
    /// Signal the scheduler to stop and wait for it to finish.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let _ = self.task.await;
    }
}

// ---------------------------------------------------------------------------
// Cron evaluation helpers
// ---------------------------------------------------------------------------

/// Convert a standard 5-field cron expression to the 6-field format expected
/// by the `cron` crate (which prepends a seconds field).
///
/// `"0 9 * * *"` → `"0 0 9 * * *"`
fn to_six_field_cron(five_field: &str) -> String {
    format!("0 {}", five_field)
}

/// Return the next fire time after `after` for the given cron schedule and
/// optional IANA timezone name. Returns `None` if the expression is invalid.
pub fn next_fire_time(
    schedule_expr: &str,
    timezone: Option<&str>,
    after: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let six_field = to_six_field_cron(schedule_expr);
    let schedule = Schedule::from_str(&six_field).ok()?;

    if let Some(tz_name) = timezone {
        let tz: chrono_tz::Tz = tz_name.parse().ok()?;
        let after_tz = after.with_timezone(&tz);
        schedule
            .after(&after_tz)
            .next()
            .map(|dt| dt.with_timezone(&Utc))
    } else {
        schedule.after(&after).next().map(|dt| dt.with_timezone(&Utc))
    }
}

/// Validate that a cron expression can be parsed. Returns `Ok(())` or an
/// error describing the problem.
pub fn validate_cron(schedule_expr: &str) -> Result<()> {
    let six_field = to_six_field_cron(schedule_expr);
    Schedule::from_str(&six_field)
        .map(|_| ())
        .with_context(|| format!("invalid cron expression: {:?}", schedule_expr))
}

/// Return `true` if the job's next fire time (since last run or creation) is
/// at or before `now`.
fn is_job_due(job: &JobDefinition, now: DateTime<Utc>) -> bool {
    if !job.enabled {
        return false;
    }
    let after = job.last_run_at.unwrap_or(job.created_at);
    match next_fire_time(&job.schedule, job.timezone.as_deref(), after) {
        Some(fire) => fire <= now,
        None => {
            warn!(job = %job.id, schedule = %job.schedule, "unparseable cron expression — skipping");
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Tick — one iteration of the scheduler loop
// ---------------------------------------------------------------------------

async fn tick(
    store: &Arc<dyn SchedulerStore>,
    workspace_store: &Arc<dyn WorkspaceStore>,
    provider: &Arc<dyn LlmProvider>,
    running: &Arc<tokio::sync::Mutex<HashSet<String>>>,
) -> Result<()> {
    let now = Utc::now();
    debug!(now = %now, "scheduler tick");

    let workspaces = workspace_store
        .list()
        .await
        .context("failed to list workspaces")?;

    for ws in workspaces {
        let jobs = match store.list_jobs(&ws.uuid).await {
            Ok(j) => j,
            Err(e) => {
                debug!(workspace = %ws.uuid, err = %e, "failed to list jobs — skipping workspace");
                continue;
            }
        };

        for job in jobs {
            if !is_job_due(&job, now) {
                continue;
            }

            // Job-level lock: skip if already running
            let lock_key = format!("{}:{}", ws.uuid, job.id);
            {
                let mut locked = running.lock().await;
                if locked.contains(&lock_key) {
                    debug!(job = %job.id, "job already running — skipping");
                    continue;
                }
                locked.insert(lock_key.clone());
            }

            info!(workspace = %ws.uuid, job = %job.id, name = %job.name, "executing scheduled job");

            // Spawn the job execution as an independent task
            let store_c = store.clone();
            let provider_c = provider.clone();
            let workspace_id = ws.uuid.clone();
            let running_c = running.clone();

            tokio::spawn(async move {
                let run = execute_job(&provider_c, &job).await;
                let status = run.status.clone();

                // Persist the run record
                if let Err(e) = store_c.write_run(&workspace_id, &job.id, &run).await {
                    error!(job = %job.id, err = %e, "failed to write job run");
                }

                // Update last_run_at on the job
                let mut updated_job = job.clone();
                updated_job.last_run_at = Some(run.timestamp);
                updated_job.updated_at = Utc::now();

                // If one_shot and successful, disable the job
                if job.one_shot && status == JobStatus::Success {
                    updated_job.enabled = false;
                    info!(job = %job.id, "one-shot job completed — disabling");
                }

                if let Err(e) = store_c.update_job(&workspace_id, &updated_job).await {
                    error!(job = %job.id, err = %e, "failed to update job after run");
                }

                // Prune old runs
                if job.max_history > 0 {
                    let _ = store_c
                        .prune_runs(&workspace_id, &job.id, job.max_history as usize)
                        .await;
                }

                // Release lock
                running_c.lock().await.remove(&lock_key);

                info!(
                    workspace = %workspace_id,
                    job = %job.id,
                    status = %status,
                    "job execution complete"
                );
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared execution engine
// ---------------------------------------------------------------------------

/// Send `prompt` to the LLM provider and collect the full response text.
///
/// Returns `Ok((output, duration_ms))` on success or
/// `Err((error_message, duration_ms))` on failure.
async fn execute_prompt(
    provider: &Arc<dyn LlmProvider>,
    prompt: &str,
) -> Result<(String, u64), (String, u64)> {
    let start = Instant::now();
    let run_id = Uuid::new_v4().to_string();

    let messages = vec![Message {
        role: Role::User,
        content: MessageContent::Text(prompt.to_string()),
    }];

    let cancel = tokio_util::sync::CancellationToken::new();
    let config = AgentLoopConfig { max_iterations: 25 };

    match run_agent_loop(
        provider.clone(),
        &messages,
        &[],
        None,
        &config,
        cancel,
        None,
        &run_id,
        None,
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
            Ok((output, start.elapsed().as_millis() as u64))
        }
        Err(e) => Err((e.to_string(), start.elapsed().as_millis() as u64)),
    }
}

// ---------------------------------------------------------------------------
// Job execution
// ---------------------------------------------------------------------------

/// Execute a single job by sending its prompt to the LLM provider.
/// Returns a [`JobRun`] recording the outcome.
async fn execute_job(provider: &Arc<dyn LlmProvider>, job: &JobDefinition) -> JobRun {
    let timestamp = Utc::now();
    info!(job = %job.id, "starting job execution");

    match execute_prompt(provider, &job.prompt).await {
        Ok((output, duration_ms)) => JobRun {
            job_id: job.id.clone(),
            timestamp,
            status: JobStatus::Success,
            output,
            duration_ms,
            error_message: None,
        },
        Err((err_msg, duration_ms)) => {
            error!(job = %job.id, err = %err_msg, "job execution failed");
            JobRun {
                job_id: job.id.clone(),
                timestamp,
                status: JobStatus::Error,
                output: String::new(),
                duration_ms,
                error_message: Some(err_msg),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn to_six_field_conversion() {
        assert_eq!(to_six_field_cron("0 9 * * *"), "0 0 9 * * *");
        assert_eq!(to_six_field_cron("30 8 * * 1"), "0 30 8 * * 1");
        assert_eq!(to_six_field_cron("0 0 1 * *"), "0 0 0 1 * *");
    }

    #[test]
    fn validate_valid_cron() {
        assert!(validate_cron("0 9 * * *").is_ok());
        assert!(validate_cron("30 8 * * 1-5").is_ok());
        assert!(validate_cron("0 0 1 1 *").is_ok());
    }

    #[test]
    fn validate_invalid_cron() {
        assert!(validate_cron("not a cron").is_err());
        assert!(validate_cron("99 99 99 99 99").is_err());
    }

    #[test]
    fn next_fire_time_utc() {
        // A job scheduled for 9 AM UTC every day
        let after = chrono::DateTime::parse_from_rfc3339("2026-02-15T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let next = next_fire_time("0 9 * * *", None, after);
        assert!(next.is_some());
        let next = next.unwrap();
        assert_eq!(next.hour(), 9);
        assert_eq!(next.minute(), 0);
    }

    #[test]
    fn next_fire_time_with_timezone() {
        // A job scheduled for 9 AM New York time
        let after = chrono::DateTime::parse_from_rfc3339("2026-02-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let next = next_fire_time("0 9 * * *", Some("America/New_York"), after);
        assert!(next.is_some());
        // 9 AM ET in Feb = 14:00 UTC (EST = UTC-5)
        assert_eq!(next.unwrap().hour(), 14);
    }

    #[test]
    fn next_fire_time_invalid_cron() {
        let after = Utc::now();
        let next = next_fire_time("not valid", None, after);
        assert!(next.is_none());
    }

    #[test]
    fn next_fire_time_invalid_timezone() {
        let after = Utc::now();
        let next = next_fire_time("0 9 * * *", Some("Invalid/Timezone"), after);
        assert!(next.is_none());
    }

    #[test]
    fn is_job_due_disabled() {
        let now = Utc::now();
        let job = JobDefinition {
            id: "j1".to_string(),
            name: "Test".to_string(),
            prompt: "Do it".to_string(),
            schedule: "0 9 * * *".to_string(),
            timezone: None,
            enabled: false, // disabled
            one_shot: false,
            notify: true,
            max_history: 7,
            created_at: now - chrono::Duration::hours(24),
            updated_at: now,
            last_run_at: None,
        };
        assert!(!is_job_due(&job, now));
    }

    #[test]
    fn is_job_due_never_run() {
        // Job created 2 hours ago with schedule "0 0 * * *" (midnight UTC)
        // If we check at 12:00 UTC the same day, it should be due (midnight passed)
        let created = chrono::DateTime::parse_from_rfc3339("2026-02-15T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            - chrono::Duration::hours(2); // created at 22:00 UTC previous day

        let now = chrono::DateTime::parse_from_rfc3339("2026-02-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        let job = JobDefinition {
            id: "j2".to_string(),
            name: "Test".to_string(),
            prompt: "Do it".to_string(),
            schedule: "0 0 * * *".to_string(),
            timezone: None,
            enabled: true,
            one_shot: false,
            notify: true,
            max_history: 7,
            created_at: created,
            updated_at: created,
            last_run_at: None,
        };

        assert!(is_job_due(&job, now));
    }
}
