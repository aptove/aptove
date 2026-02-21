//! JobService — orchestration logic for scheduled jobs.
//!
//! Encapsulates the business logic that currently lives in the CLI
//! `jobs/*` handler functions, making it testable and reusable.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::agent_loop::{run_agent_loop, AgentLoopConfig};
use crate::builder::AgentRuntime;
use crate::provider::LlmProvider;
use crate::scheduler::{JobDefinition, JobRun, JobStatus, SchedulerStore};
use crate::types::{Message, MessageContent, Role};
use crate::workspace::WorkspaceStore;

// ---------------------------------------------------------------------------
// JobCreateOptions
// ---------------------------------------------------------------------------

/// Optional parameters for creating a new scheduled job.
#[derive(Debug, Clone)]
pub struct JobCreateOptions {
    pub timezone: Option<String>,
    pub one_shot: bool,
    pub notify: bool,
    pub max_history: u32,
}

impl Default for JobCreateOptions {
    fn default() -> Self {
        Self {
            timezone: None,
            one_shot: false,
            notify: true,
            max_history: 7,
        }
    }
}

// ---------------------------------------------------------------------------
// JobService
// ---------------------------------------------------------------------------

pub struct JobService {
    scheduler_store: Arc<dyn SchedulerStore>,
    #[allow(dead_code)]
    workspace_store: Arc<dyn WorkspaceStore>,
}

impl JobService {
    pub fn new(
        scheduler_store: Arc<dyn SchedulerStore>,
        workspace_store: Arc<dyn WorkspaceStore>,
    ) -> Self {
        Self {
            scheduler_store,
            workspace_store,
        }
    }

    /// Create a new scheduled job: validate cron expression, then persist.
    pub async fn create(
        &self,
        workspace_id: &str,
        name: &str,
        prompt: &str,
        schedule: &str,
        options: JobCreateOptions,
    ) -> Result<JobDefinition> {
        validate_cron_expr(schedule)
            .with_context(|| format!("invalid cron expression: {:?}", schedule))?;

        let now = Utc::now();
        let job = JobDefinition {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            prompt: prompt.to_string(),
            schedule: schedule.to_string(),
            timezone: options.timezone,
            enabled: true,
            one_shot: options.one_shot,
            notify: options.notify,
            max_history: options.max_history,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        };

        self.scheduler_store
            .create_job(workspace_id, &job)
            .await
            .context("failed to create job")
    }

    /// Trigger a job immediately: run the agent loop, write the run record,
    /// update last_run_at, handle one_shot disabling, prune old runs.
    pub async fn trigger_now(
        &self,
        workspace_id: &str,
        job_id: &str,
        provider: Arc<dyn LlmProvider>,
        runtime: &AgentRuntime,
    ) -> Result<JobRun> {
        let job = self
            .scheduler_store
            .get_job(workspace_id, job_id)
            .await
            .context("job not found")?;

        let messages = vec![Message {
            role: Role::User,
            content: MessageContent::Text(job.prompt.clone()),
        }];
        let cancel = tokio_util::sync::CancellationToken::new();
        let run_id = Uuid::new_v4().to_string();
        let loop_cfg = AgentLoopConfig {
            max_iterations: 25,
            ..AgentLoopConfig::default()
        };
        let start = Instant::now();
        let timestamp = Utc::now();

        let run = match run_agent_loop(
            provider,
            &messages,
            &[],
            None,
            &loop_cfg,
            cancel,
            None,
            &run_id,
            Some(runtime),
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
            Err(e) => JobRun {
                job_id: job.id.clone(),
                timestamp,
                status: JobStatus::Error,
                output: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                error_message: Some(e.to_string()),
            },
        };

        self.scheduler_store
            .write_run(workspace_id, &job.id, &run)
            .await
            .ok();

        let mut updated_job = job.clone();
        updated_job.last_run_at = Some(run.timestamp);
        updated_job.updated_at = Utc::now();
        if job.one_shot && run.status == JobStatus::Success {
            updated_job.enabled = false;
        }
        self.scheduler_store
            .update_job(workspace_id, &updated_job)
            .await
            .ok();

        if job.max_history > 0 {
            self.scheduler_store
                .prune_runs(workspace_id, &job.id, job.max_history as usize)
                .await
                .ok();
        }

        Ok(run)
    }
}

// ---------------------------------------------------------------------------
// Cron validation (5-field format: min hour dom month dow)
// ---------------------------------------------------------------------------

fn validate_cron_expr(schedule: &str) -> Result<()> {
    // The `cron` crate requires 6 fields (sec min hour dom month dow).
    // Prepend "0" to convert 5-field user input to the 6-field form.
    let six_field = format!("0 {}", schedule);
    cron::Schedule::from_str(&six_field)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("{}", e))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::{TriggerDefinition, TriggerRun, TriggerStore};
    use crate::workspace::{WorkspaceMetadata, WorkspaceStore, WorkspaceSummary};
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    // -- Mock SchedulerStore -----------------------------------------------

    struct MockSchedulerStore {
        jobs: RwLock<HashMap<String, JobDefinition>>,
        runs: RwLock<Vec<JobRun>>,
    }

    impl MockSchedulerStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                jobs: RwLock::new(HashMap::new()),
                runs: RwLock::new(vec![]),
            })
        }
    }

    #[async_trait::async_trait]
    impl SchedulerStore for MockSchedulerStore {
        async fn create_job(&self, _ws: &str, job: &JobDefinition) -> Result<JobDefinition> {
            self.jobs
                .write()
                .await
                .insert(job.id.clone(), job.clone());
            Ok(job.clone())
        }
        async fn get_job(&self, _ws: &str, id: &str) -> Result<JobDefinition> {
            self.jobs
                .read()
                .await
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("job not found: {}", id))
        }
        async fn list_jobs(&self, _ws: &str) -> Result<Vec<JobDefinition>> {
            Ok(self.jobs.read().await.values().cloned().collect())
        }
        async fn update_job(&self, _ws: &str, job: &JobDefinition) -> Result<JobDefinition> {
            self.jobs
                .write()
                .await
                .insert(job.id.clone(), job.clone());
            Ok(job.clone())
        }
        async fn delete_job(&self, _ws: &str, id: &str) -> Result<()> {
            self.jobs.write().await.remove(id);
            Ok(())
        }
        async fn write_run(&self, _ws: &str, _jid: &str, run: &JobRun) -> Result<()> {
            self.runs.write().await.push(run.clone());
            Ok(())
        }
        async fn list_runs(
            &self,
            _ws: &str,
            _jid: &str,
            limit: usize,
        ) -> Result<Vec<JobRun>> {
            let runs = self.runs.read().await;
            Ok(runs.iter().take(limit).cloned().collect())
        }
        async fn prune_runs(&self, _ws: &str, _jid: &str, _max: usize) -> Result<u64> {
            Ok(0)
        }
    }

    // -- Mock WorkspaceStore (minimal) -------------------------------------

    struct MockWorkspaceStore;

    #[async_trait::async_trait]
    impl WorkspaceStore for MockWorkspaceStore {
        async fn create(
            &self,
            uuid: &str,
            name: Option<&str>,
            provider: Option<&str>,
        ) -> Result<WorkspaceMetadata> {
            Ok(WorkspaceMetadata {
                uuid: uuid.to_string(),
                name: name.map(|s| s.to_string()),
                created_at: Utc::now(),
                last_accessed: Utc::now(),
                provider: provider.map(|s| s.to_string()),
            })
        }
        async fn load(&self, uuid: &str) -> Result<WorkspaceMetadata> {
            Err(anyhow::anyhow!("not found: {}", uuid))
        }
        async fn list(&self) -> Result<Vec<WorkspaceSummary>> {
            Ok(vec![])
        }
        async fn delete(&self, _uuid: &str) -> Result<()> {
            Ok(())
        }
        async fn update_accessed(&self, _uuid: &str) -> Result<()> {
            Ok(())
        }
        async fn gc(&self, _max_age_days: u64) -> Result<u64> {
            Ok(0)
        }
        async fn load_config(&self, _uuid: &str) -> Result<Option<String>> {
            Ok(None)
        }
    }

    // -- Unused mock for TriggerStore (needed by service constructor pattern) --
    // (Not actually used in these tests — trigger_service tests have their own)
    #[allow(dead_code)]
    struct MockTriggerStore;

    #[allow(dead_code)]
    #[async_trait::async_trait]
    impl TriggerStore for MockTriggerStore {
        async fn create_trigger(
            &self,
            _ws: &str,
            t: &TriggerDefinition,
        ) -> Result<TriggerDefinition> {
            Ok(t.clone())
        }
        async fn get_trigger(&self, _ws: &str, _id: &str) -> Result<TriggerDefinition> {
            Err(anyhow::anyhow!("not found"))
        }
        async fn list_triggers(&self, _ws: &str) -> Result<Vec<TriggerDefinition>> {
            Ok(vec![])
        }
        async fn update_trigger(
            &self,
            _ws: &str,
            t: &TriggerDefinition,
        ) -> Result<TriggerDefinition> {
            Ok(t.clone())
        }
        async fn delete_trigger(&self, _ws: &str, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn resolve_token(
            &self,
            _token: &str,
        ) -> Result<Option<(String, TriggerDefinition)>> {
            Ok(None)
        }
        async fn write_run(&self, _ws: &str, _tid: &str, _run: &TriggerRun) -> Result<()> {
            Ok(())
        }
        async fn list_runs(
            &self,
            _ws: &str,
            _tid: &str,
            _limit: usize,
        ) -> Result<Vec<TriggerRun>> {
            Ok(vec![])
        }
        async fn prune_runs(&self, _ws: &str, _tid: &str, _max: usize) -> Result<u64> {
            Ok(0)
        }
        async fn regenerate_token(
            &self,
            _ws: &str,
            _id: &str,
        ) -> Result<TriggerDefinition> {
            Err(anyhow::anyhow!("not found"))
        }
    }

    // -- Helpers -----------------------------------------------------------

    fn make_service() -> JobService {
        JobService::new(MockSchedulerStore::new(), Arc::new(MockWorkspaceStore))
    }

    // -- Tests -------------------------------------------------------------

    #[test]
    fn valid_cron_expressions_pass() {
        assert!(validate_cron_expr("0 9 * * *").is_ok());
        assert!(validate_cron_expr("30 8 * * 1-5").is_ok());
        assert!(validate_cron_expr("0 0 1 1 *").is_ok());
    }

    #[test]
    fn invalid_cron_expressions_fail() {
        assert!(validate_cron_expr("not a cron").is_err());
        assert!(validate_cron_expr("99 99 99 99 99").is_err());
    }

    #[tokio::test]
    async fn create_persists_job() {
        let svc = make_service();
        let job = svc
            .create(
                "ws1",
                "Morning Summary",
                "Summarize my emails",
                "0 9 * * *",
                JobCreateOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(job.name, "Morning Summary");
        assert_eq!(job.schedule, "0 9 * * *");
        assert!(job.enabled);
        assert_eq!(job.max_history, 7);
        assert!(!job.one_shot);
    }

    #[tokio::test]
    async fn create_rejects_invalid_cron() {
        let svc = make_service();
        let result = svc
            .create("ws1", "Bad", "prompt", "not a cron", Default::default())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid cron"));
    }

    #[tokio::test]
    async fn create_respects_options() {
        let svc = make_service();
        let opts = JobCreateOptions {
            timezone: Some("America/New_York".to_string()),
            one_shot: true,
            notify: false,
            max_history: 3,
        };
        let job = svc
            .create("ws1", "J", "do it", "0 9 * * *", opts)
            .await
            .unwrap();

        assert_eq!(job.timezone, Some("America/New_York".to_string()));
        assert!(job.one_shot);
        assert!(!job.notify);
        assert_eq!(job.max_history, 3);
    }

    #[tokio::test]
    async fn trigger_now_missing_job_errors() {
        use crate::bindings::BindingStore;
        use crate::config::AgentConfig;
        use crate::persistence::{SessionData, SessionStore};
        use crate::provider::{LlmProvider, LlmResponse, ModelInfo, StopReason};
        use crate::types::{Message, ToolDefinition};
        use crate::{AgentBuilder, AgentRuntime};

        struct DummyProvider;
        #[async_trait::async_trait]
        impl LlmProvider for DummyProvider {
            fn name(&self) -> &str {
                "dummy"
            }
            async fn complete(
                &self,
                _msgs: &[Message],
                _tools: &[ToolDefinition],
                _cb: Option<crate::provider::StreamCallback>,
            ) -> Result<LlmResponse> {
                Ok(LlmResponse {
                    content: "ok".to_string(),
                    stop_reason: StopReason::EndTurn,
                    usage: crate::provider::TokenUsage::default(),
                    tool_calls: vec![],
                })
            }
            fn model_info(&self) -> ModelInfo {
                ModelInfo {
                    name: "dummy".to_string(),
                    max_context_tokens: 4096,
                    max_output_tokens: 4096,
                    provider_name: "dummy".to_string(),
                }
            }
        }

        struct MockSS;
        #[async_trait::async_trait]
        impl SessionStore for MockSS {
            async fn read(&self, _: &str) -> Result<SessionData> {
                Ok(SessionData::new("dummy", "dummy"))
            }
            async fn append(&self, _: &str, _: &Message) -> Result<()> {
                Ok(())
            }
            async fn write(&self, _: &str, _: &SessionData) -> Result<()> {
                Ok(())
            }
            async fn clear(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }

        struct MockBS;
        #[async_trait::async_trait]
        impl BindingStore for MockBS {
            async fn resolve(&self, _: &str) -> Result<Option<String>> {
                Ok(None)
            }
            async fn bind(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            async fn unbind(&self, _: &str) -> Result<()> {
                Ok(())
            }
            async fn unbind_workspace(&self, _: &str) -> Result<()> {
                Ok(())
            }
        }

        let runtime: AgentRuntime = AgentBuilder::new(AgentConfig::default())
            .with_llm(Arc::new(DummyProvider))
            .with_workspace_store(Arc::new(MockWorkspaceStore))
            .with_session_store(Arc::new(MockSS))
            .with_binding_store(Arc::new(MockBS))
            .build()
            .unwrap();

        let svc = make_service();
        let result = svc
            .trigger_now("ws1", "nonexistent-id", Arc::new(DummyProvider), &runtime)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("job not found"));
    }
}
