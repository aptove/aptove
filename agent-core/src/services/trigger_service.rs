//! TriggerService — orchestration logic for webhook triggers.
//!
//! Encapsulates the business logic that currently lives in the CLI
//! `triggers/*` handler functions, making it testable and reusable.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use uuid::Uuid;

use crate::agent_loop::{run_agent_loop, AgentLoopConfig};
use crate::builder::AgentRuntime;
use crate::provider::LlmProvider;
use crate::scheduler::SchedulerStore;
use crate::trigger::{
    generate_token, render_template, TriggerDefinition, TriggerEvent, TriggerRun, TriggerStatus,
    TriggerStore,
};
use crate::types::MessageContent;
use crate::workspace::WorkspaceStore;

// ---------------------------------------------------------------------------
// TriggerCreateOptions
// ---------------------------------------------------------------------------

/// Optional parameters for creating a new trigger.
#[derive(Debug, Clone)]
pub struct TriggerCreateOptions {
    pub description: Option<String>,
    pub max_history: u32,
    pub rate_limit_per_minute: u32,
    pub notify: bool,
    pub hmac_secret: Option<String>,
    pub accepted_content_types: Vec<String>,
}

impl Default for TriggerCreateOptions {
    fn default() -> Self {
        Self {
            description: None,
            max_history: 20,
            rate_limit_per_minute: 60,
            notify: true,
            hmac_secret: None,
            accepted_content_types: vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// TriggerService
// ---------------------------------------------------------------------------

pub struct TriggerService {
    trigger_store: Arc<dyn TriggerStore>,
    #[allow(dead_code)]
    scheduler_store: Arc<dyn SchedulerStore>,
    #[allow(dead_code)]
    workspace_store: Arc<dyn WorkspaceStore>,
}

impl TriggerService {
    pub fn new(
        trigger_store: Arc<dyn TriggerStore>,
        scheduler_store: Arc<dyn SchedulerStore>,
        workspace_store: Arc<dyn WorkspaceStore>,
    ) -> Self {
        Self {
            trigger_store,
            scheduler_store,
            workspace_store,
        }
    }

    /// Create a new webhook trigger: validate inputs, generate token, persist.
    pub async fn create(
        &self,
        workspace_id: &str,
        name: &str,
        prompt_template: &str,
        options: TriggerCreateOptions,
    ) -> Result<TriggerDefinition> {
        let now = Utc::now();
        let trigger = TriggerDefinition {
            id: Uuid::new_v4().to_string(),
            name: name.to_string(),
            prompt_template: prompt_template.to_string(),
            token: generate_token(),
            enabled: true,
            notify: options.notify,
            max_history: options.max_history,
            rate_limit_per_minute: options.rate_limit_per_minute,
            hmac_secret: options.hmac_secret,
            accepted_content_types: options.accepted_content_types,
            created_at: now,
            updated_at: now,
            last_event_at: None,
        };
        self.trigger_store
            .create_trigger(workspace_id, &trigger)
            .await
            .context("failed to create trigger")
    }

    /// Execute an incoming webhook event: get trigger, render template, run
    /// the agent loop, persist the run record, update last_event_at, prune.
    pub async fn execute_event(
        &self,
        event: &TriggerEvent,
        provider: Arc<dyn LlmProvider>,
        runtime: &AgentRuntime,
    ) -> Result<TriggerRun> {
        let trigger = self
            .trigger_store
            .get_trigger(&event.workspace_id, &event.trigger_id)
            .await
            .context("trigger not found")?;

        let rendered_prompt = render_template(&trigger.prompt_template, event, &trigger.name);

        let messages = vec![crate::types::Message {
            role: crate::types::Role::User,
            content: MessageContent::Text(rendered_prompt),
        }];
        let cancel = tokio_util::sync::CancellationToken::new();
        let run_id = Uuid::new_v4().to_string();
        let loop_cfg = AgentLoopConfig::default();
        let start = Instant::now();
        let payload_preview: String = event.payload.chars().take(200).collect();

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
                TriggerRun {
                    trigger_id: trigger.id.clone(),
                    timestamp: event.received_at,
                    status: TriggerStatus::Success,
                    payload_preview,
                    output,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error_message: None,
                    source_ip: None,
                }
            }
            Err(e) => TriggerRun {
                trigger_id: trigger.id.clone(),
                timestamp: event.received_at,
                status: TriggerStatus::Error,
                payload_preview,
                output: String::new(),
                duration_ms: start.elapsed().as_millis() as u64,
                error_message: Some(e.to_string()),
                source_ip: None,
            },
        };

        self.trigger_store
            .write_run(&event.workspace_id, &trigger.id, &run)
            .await
            .ok();

        // Update last_event_at
        let mut updated = trigger.clone();
        updated.last_event_at = Some(Utc::now());
        updated.updated_at = Utc::now();
        self.trigger_store
            .update_trigger(&event.workspace_id, &updated)
            .await
            .ok();

        // Prune old run records
        if trigger.max_history > 0 {
            self.trigger_store
                .prune_runs(
                    &event.workspace_id,
                    &trigger.id,
                    trigger.max_history as usize,
                )
                .await
                .ok();
        }

        Ok(run)
    }

    /// Generate a new webhook token for an existing trigger.
    pub async fn regenerate_token(
        &self,
        workspace_id: &str,
        trigger_id: &str,
    ) -> Result<TriggerDefinition> {
        self.trigger_store
            .regenerate_token(workspace_id, trigger_id)
            .await
            .context("failed to regenerate token")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::SchedulerStore;
    use crate::scheduler::{JobDefinition, JobRun};
    use crate::trigger::{TriggerRun, TriggerStore};
    use crate::workspace::{WorkspaceMetadata, WorkspaceStore, WorkspaceSummary};
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    // -- Mock TriggerStore -------------------------------------------------

    struct MockTriggerStore {
        triggers: RwLock<HashMap<String, TriggerDefinition>>,
        runs: RwLock<Vec<TriggerRun>>,
    }

    impl MockTriggerStore {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                triggers: RwLock::new(HashMap::new()),
                runs: RwLock::new(vec![]),
            })
        }
    }

    #[async_trait::async_trait]
    impl TriggerStore for MockTriggerStore {
        async fn create_trigger(
            &self,
            _ws: &str,
            trigger: &TriggerDefinition,
        ) -> Result<TriggerDefinition> {
            self.triggers
                .write()
                .await
                .insert(trigger.id.clone(), trigger.clone());
            Ok(trigger.clone())
        }

        async fn get_trigger(&self, _ws: &str, id: &str) -> Result<TriggerDefinition> {
            self.triggers
                .read()
                .await
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("trigger not found"))
        }

        async fn list_triggers(&self, _ws: &str) -> Result<Vec<TriggerDefinition>> {
            Ok(self.triggers.read().await.values().cloned().collect())
        }

        async fn update_trigger(
            &self,
            _ws: &str,
            trigger: &TriggerDefinition,
        ) -> Result<TriggerDefinition> {
            self.triggers
                .write()
                .await
                .insert(trigger.id.clone(), trigger.clone());
            Ok(trigger.clone())
        }

        async fn delete_trigger(&self, _ws: &str, id: &str) -> Result<()> {
            self.triggers.write().await.remove(id);
            Ok(())
        }

        async fn resolve_token(
            &self,
            token: &str,
        ) -> Result<Option<(String, TriggerDefinition)>> {
            let map = self.triggers.read().await;
            for t in map.values() {
                if t.token == token && t.enabled {
                    return Ok(Some(("ws".to_string(), t.clone())));
                }
            }
            Ok(None)
        }

        async fn write_run(&self, _ws: &str, _tid: &str, run: &TriggerRun) -> Result<()> {
            self.runs.write().await.push(run.clone());
            Ok(())
        }

        async fn list_runs(&self, _ws: &str, _tid: &str, limit: usize) -> Result<Vec<TriggerRun>> {
            let runs = self.runs.read().await;
            Ok(runs.iter().take(limit).cloned().collect())
        }

        async fn prune_runs(&self, _ws: &str, _tid: &str, _max: usize) -> Result<u64> {
            Ok(0)
        }

        async fn regenerate_token(
            &self,
            _ws: &str,
            id: &str,
        ) -> Result<TriggerDefinition> {
            let mut map = self.triggers.write().await;
            let t = map
                .get_mut(id)
                .ok_or_else(|| anyhow::anyhow!("trigger not found"))?;
            t.token = generate_token();
            Ok(t.clone())
        }
    }

    // -- Mock SchedulerStore (minimal) -------------------------------------

    struct MockSchedulerStore;

    #[async_trait::async_trait]
    impl SchedulerStore for MockSchedulerStore {
        async fn create_job(&self, _ws: &str, job: &JobDefinition) -> Result<JobDefinition> {
            Ok(job.clone())
        }
        async fn get_job(&self, _ws: &str, _id: &str) -> Result<JobDefinition> {
            Err(anyhow::anyhow!("not found"))
        }
        async fn list_jobs(&self, _ws: &str) -> Result<Vec<JobDefinition>> {
            Ok(vec![])
        }
        async fn update_job(&self, _ws: &str, job: &JobDefinition) -> Result<JobDefinition> {
            Ok(job.clone())
        }
        async fn delete_job(&self, _ws: &str, _id: &str) -> Result<()> {
            Ok(())
        }
        async fn write_run(&self, _ws: &str, _jid: &str, _run: &JobRun) -> Result<()> {
            Ok(())
        }
        async fn list_runs(&self, _ws: &str, _jid: &str, _limit: usize) -> Result<Vec<JobRun>> {
            Ok(vec![])
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
            Err(anyhow::anyhow!("workspace not found: {}", uuid))
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

    // -- Helpers -----------------------------------------------------------

    fn make_service() -> TriggerService {
        TriggerService::new(
            MockTriggerStore::new(),
            Arc::new(MockSchedulerStore),
            Arc::new(MockWorkspaceStore),
        )
    }

    // -- Tests -------------------------------------------------------------

    #[tokio::test]
    async fn create_generates_token_and_persists() {
        let svc = make_service();
        let trigger = svc
            .create(
                "ws1",
                "My Trigger",
                "Summarize: {{payload}}",
                TriggerCreateOptions::default(),
            )
            .await
            .unwrap();

        assert_eq!(trigger.name, "My Trigger");
        assert!(trigger.token.starts_with("wh_"));
        assert!(trigger.enabled);
        assert_eq!(trigger.max_history, 20);
        assert_eq!(trigger.rate_limit_per_minute, 60);
        assert!(trigger.notify);
    }

    #[tokio::test]
    async fn create_respects_options() {
        let svc = make_service();
        let opts = TriggerCreateOptions {
            max_history: 5,
            rate_limit_per_minute: 10,
            notify: false,
            hmac_secret: Some("secret".to_string()),
            accepted_content_types: vec!["application/json".to_string()],
            description: None,
        };
        let trigger = svc
            .create("ws1", "T", "{{payload}}", opts)
            .await
            .unwrap();

        assert_eq!(trigger.max_history, 5);
        assert_eq!(trigger.rate_limit_per_minute, 10);
        assert!(!trigger.notify);
        assert_eq!(trigger.hmac_secret, Some("secret".to_string()));
        assert_eq!(trigger.accepted_content_types, vec!["application/json"]);
    }

    #[tokio::test]
    async fn create_ids_are_unique() {
        let svc = make_service();
        let t1 = svc
            .create("ws1", "T1", "{{payload}}", Default::default())
            .await
            .unwrap();
        let t2 = svc
            .create("ws1", "T2", "{{payload}}", Default::default())
            .await
            .unwrap();
        assert_ne!(t1.id, t2.id);
        assert_ne!(t1.token, t2.token);
    }

    #[tokio::test]
    async fn regenerate_token_changes_token() {
        let svc = make_service();
        let trigger = svc
            .create("ws1", "T", "{{payload}}", Default::default())
            .await
            .unwrap();
        let old_token = trigger.token.clone();

        let updated = svc
            .regenerate_token("ws1", &trigger.id)
            .await
            .unwrap();

        assert_ne!(updated.token, old_token);
        assert!(updated.token.starts_with("wh_"));
    }

    #[tokio::test]
    async fn regenerate_token_missing_trigger_errors() {
        let svc = make_service();
        let result = svc.regenerate_token("ws1", "nonexistent").await;
        assert!(result.is_err());
    }
}
