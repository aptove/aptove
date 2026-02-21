//! Scheduled Job Types and `SchedulerStore` Trait
//!
//! Defines the data model for scheduled jobs and the storage trait that
//! backends (filesystem, database, etc.) must implement.
//!
//! The scheduler engine itself lives in `agent-scheduler` to keep this crate
//! lean (no `cron` or `chrono-tz` dependency here).

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// JobStatus
// ---------------------------------------------------------------------------

/// Status of a job or individual job run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Waiting to run (never executed yet).
    Pending,
    /// Currently executing.
    Running,
    /// Last run completed successfully.
    Success,
    /// Last run encountered an error.
    Error,
    /// Job is disabled (`enabled = false`).
    Disabled,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobStatus::Pending => write!(f, "pending"),
            JobStatus::Running => write!(f, "running"),
            JobStatus::Success => write!(f, "success"),
            JobStatus::Error => write!(f, "error"),
            JobStatus::Disabled => write!(f, "disabled"),
        }
    }
}

// ---------------------------------------------------------------------------
// JobDefinition
// ---------------------------------------------------------------------------

/// A complete scheduled job definition (the on-disk / trait boundary type).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDefinition {
    /// UUID for this job (set by the store on create).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// The prompt text sent to the LLM on each run.
    pub prompt: String,
    /// Standard 5-field cron expression in the job's timezone (e.g. `"0 9 * * *"`).
    pub schedule: String,
    /// IANA timezone name (e.g. `"America/New_York"`). `None` → UTC.
    pub timezone: Option<String>,
    /// Whether the scheduler should fire this job.
    pub enabled: bool,
    /// If `true`, disable the job after its first successful run.
    pub one_shot: bool,
    /// Whether to send a push notification on completion.
    pub notify: bool,
    /// Keep the last N run records (0 = keep all).
    pub max_history: u32,
    /// When the job was created.
    pub created_at: DateTime<Utc>,
    /// When the job was last modified.
    pub updated_at: DateTime<Utc>,
    /// When the job last executed (None if never run).
    pub last_run_at: Option<DateTime<Utc>>,
}

impl JobDefinition {
    /// Return the derived status based on current fields (no run history available).
    pub fn status(&self) -> JobStatus {
        if !self.enabled {
            JobStatus::Disabled
        } else if self.last_run_at.is_none() {
            JobStatus::Pending
        } else {
            // Can't determine run outcome without JobRun data; caller should
            // consult the most recent JobRun when possible.
            JobStatus::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// JobSummary
// ---------------------------------------------------------------------------

/// Lightweight view of a job for list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: String,
    pub name: String,
    pub schedule: String,
    pub timezone: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_status: Option<JobStatus>,
}

impl From<&JobDefinition> for JobSummary {
    fn from(j: &JobDefinition) -> Self {
        Self {
            id: j.id.clone(),
            name: j.name.clone(),
            schedule: j.schedule.clone(),
            timezone: j.timezone.clone(),
            enabled: j.enabled,
            last_run_at: j.last_run_at,
            last_status: if !j.enabled {
                Some(JobStatus::Disabled)
            } else if j.last_run_at.is_none() {
                Some(JobStatus::Pending)
            } else {
                None
            },
        }
    }
}

// ---------------------------------------------------------------------------
// JobRun
// ---------------------------------------------------------------------------

/// A record of a single job execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRun {
    /// ID of the parent job.
    pub job_id: String,
    /// When the run started (also used as the unique run identifier).
    pub timestamp: DateTime<Utc>,
    /// Final status of this run.
    pub status: JobStatus,
    /// Full LLM response text for this run.
    pub output: String,
    /// Wall-clock duration of the run in milliseconds.
    pub duration_ms: u64,
    /// Set when `status == Error`.
    pub error_message: Option<String>,
}

// ---------------------------------------------------------------------------
// SchedulerStore
// ---------------------------------------------------------------------------

/// Storage backend for scheduled jobs and their run history.
///
/// All operations are scoped to a workspace UUID. Implementations are
/// responsible for persistence (filesystem, database, etc.).
#[async_trait]
pub trait SchedulerStore: Send + Sync {
    /// Create a new job in a workspace. The store assigns the UUID and
    /// timestamps; callers set `name`, `prompt`, `schedule`, etc.
    async fn create_job(&self, workspace_id: &str, job: &JobDefinition) -> Result<JobDefinition>;

    /// Load a job by its UUID.
    async fn get_job(&self, workspace_id: &str, job_id: &str) -> Result<JobDefinition>;

    /// List all jobs for a workspace (order unspecified).
    async fn list_jobs(&self, workspace_id: &str) -> Result<Vec<JobDefinition>>;

    /// Persist a modified job definition (enable/disable, schedule change, etc.).
    async fn update_job(&self, workspace_id: &str, job: &JobDefinition) -> Result<JobDefinition>;

    /// Delete a job and all of its run history.
    async fn delete_job(&self, workspace_id: &str, job_id: &str) -> Result<()>;

    /// Append a run record for a job.
    async fn write_run(&self, workspace_id: &str, job_id: &str, run: &JobRun) -> Result<()>;

    /// Return up to `limit` most-recent runs for a job (most recent first).
    /// Pass `usize::MAX` to return all runs.
    async fn list_runs(
        &self,
        workspace_id: &str,
        job_id: &str,
        limit: usize,
    ) -> Result<Vec<JobRun>>;

    /// Remove the oldest runs that exceed `max_history`. Returns count deleted.
    /// A `max_history` of 0 means keep all runs (no pruning).
    async fn prune_runs(
        &self,
        workspace_id: &str,
        job_id: &str,
        max_history: usize,
    ) -> Result<u64>;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_status_display() {
        assert_eq!(JobStatus::Pending.to_string(), "pending");
        assert_eq!(JobStatus::Running.to_string(), "running");
        assert_eq!(JobStatus::Success.to_string(), "success");
        assert_eq!(JobStatus::Error.to_string(), "error");
        assert_eq!(JobStatus::Disabled.to_string(), "disabled");
    }

    #[test]
    fn job_status_serde_roundtrip() {
        let s = serde_json::to_string(&JobStatus::Success).unwrap();
        assert_eq!(s, "\"success\"");
        let parsed: JobStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, JobStatus::Success);
    }

    #[test]
    fn job_summary_from_disabled() {
        let now = Utc::now();
        let job = JobDefinition {
            id: "test-id".to_string(),
            name: "Test Job".to_string(),
            prompt: "Do something".to_string(),
            schedule: "0 9 * * *".to_string(),
            timezone: None,
            enabled: false,
            one_shot: false,
            notify: true,
            max_history: 7,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        };

        let summary = JobSummary::from(&job);
        assert_eq!(summary.id, "test-id");
        assert!(!summary.enabled);
        assert_eq!(summary.last_status, Some(JobStatus::Disabled));
    }

    #[test]
    fn job_summary_from_pending() {
        let now = Utc::now();
        let job = JobDefinition {
            id: "test-id".to_string(),
            name: "Test Job".to_string(),
            prompt: "Do something".to_string(),
            schedule: "0 9 * * *".to_string(),
            timezone: Some("America/New_York".to_string()),
            enabled: true,
            one_shot: true,
            notify: false,
            max_history: 3,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        };

        let summary = JobSummary::from(&job);
        assert!(summary.enabled);
        assert_eq!(summary.last_status, Some(JobStatus::Pending));
        assert_eq!(summary.timezone, Some("America/New_York".to_string()));
    }

    #[test]
    fn job_definition_serde_roundtrip() {
        let now = Utc::now();
        let job = JobDefinition {
            id: "abc-123".to_string(),
            name: "Morning Summary".to_string(),
            prompt: "Summarize my emails".to_string(),
            schedule: "0 9 * * *".to_string(),
            timezone: Some("UTC".to_string()),
            enabled: true,
            one_shot: false,
            notify: true,
            max_history: 7,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        };

        let json = serde_json::to_string(&job).unwrap();
        let parsed: JobDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, job.id);
        assert_eq!(parsed.name, job.name);
        assert_eq!(parsed.schedule, job.schedule);
        assert_eq!(parsed.enabled, job.enabled);
    }
}
