//! Filesystem implementation of `SchedulerStore`.
//!
//! Job folder layout under `<data_dir>/workspaces/<workspace_id>/jobs/`:
//!
//! ```text
//! jobs/
//! ├── <job-uuid>/
//! │   ├── job.toml         # JobDefinition (TOML)
//! │   ├── output.md        # Latest run output (plain text)
//! │   └── runs/
//! │       ├── 2026-02-15T09:00:00Z.json   # JobRun records (JSON)
//! │       └── ...
//! └── ...
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use agent_core::scheduler::{JobDefinition, JobRun, SchedulerStore};

// ---------------------------------------------------------------------------
// On-disk TOML representation of JobDefinition
// ---------------------------------------------------------------------------

/// Matches the `job.toml` file format.  We store timestamps as RFC-3339
/// strings so the file is human-readable without a special editor.
#[derive(Debug, Serialize, Deserialize)]
struct JobToml {
    id: String,
    name: String,
    prompt: String,
    schedule: String,
    #[serde(default)]
    timezone: Option<String>,
    enabled: bool,
    one_shot: bool,
    notify: bool,
    max_history: u32,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    last_run_at: Option<String>,
}

impl JobToml {
    fn from_def(j: &JobDefinition) -> Self {
        Self {
            id: j.id.clone(),
            name: j.name.clone(),
            prompt: j.prompt.clone(),
            schedule: j.schedule.clone(),
            timezone: j.timezone.clone(),
            enabled: j.enabled,
            one_shot: j.one_shot,
            notify: j.notify,
            max_history: j.max_history,
            created_at: j.created_at.to_rfc3339(),
            updated_at: j.updated_at.to_rfc3339(),
            last_run_at: j.last_run_at.map(|t| t.to_rfc3339()),
        }
    }

    fn into_def(self) -> Result<JobDefinition> {
        let parse = |s: &str| -> Result<DateTime<Utc>> {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .with_context(|| format!("invalid datetime: {}", s))
        };
        Ok(JobDefinition {
            id: self.id,
            name: self.name,
            prompt: self.prompt,
            schedule: self.schedule,
            timezone: self.timezone,
            enabled: self.enabled,
            one_shot: self.one_shot,
            notify: self.notify,
            max_history: self.max_history,
            created_at: parse(&self.created_at)?,
            updated_at: parse(&self.updated_at)?,
            last_run_at: self.last_run_at.as_deref().map(parse).transpose()?,
        })
    }
}

// ---------------------------------------------------------------------------
// FsSchedulerStore
// ---------------------------------------------------------------------------

/// Filesystem-backed scheduler store.
pub struct FsSchedulerStore {
    data_dir: PathBuf,
}

impl FsSchedulerStore {
    /// Create a store rooted at `data_dir` (the same root used by other Fs stores).
    pub fn new(data_dir: &Path) -> Result<Self> {
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
        })
    }

    // -- Path helpers --------------------------------------------------------

    fn jobs_dir(&self, workspace_id: &str) -> PathBuf {
        self.data_dir
            .join("workspaces")
            .join(workspace_id)
            .join("jobs")
    }

    fn job_dir(&self, workspace_id: &str, job_id: &str) -> PathBuf {
        self.jobs_dir(workspace_id).join(job_id)
    }

    fn job_toml_path(&self, workspace_id: &str, job_id: &str) -> PathBuf {
        self.job_dir(workspace_id, job_id).join("job.toml")
    }

    fn output_md_path(&self, workspace_id: &str, job_id: &str) -> PathBuf {
        self.job_dir(workspace_id, job_id).join("output.md")
    }

    fn runs_dir(&self, workspace_id: &str, job_id: &str) -> PathBuf {
        self.job_dir(workspace_id, job_id).join("runs")
    }

    fn run_path(&self, workspace_id: &str, job_id: &str, timestamp: &DateTime<Utc>) -> PathBuf {
        // Filesystem-safe timestamp: replace `:` with `-`
        let ts_safe = timestamp.to_rfc3339().replace(':', "-");
        self.runs_dir(workspace_id, job_id)
            .join(format!("{}.json", ts_safe))
    }

    // -- Sync helpers (used inside async fn via spawn_blocking or directly) --

    fn read_job_sync(&self, workspace_id: &str, job_id: &str) -> Result<JobDefinition> {
        let path = self.job_toml_path(workspace_id, job_id);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read job.toml: {}", path.display()))?;
        let job_toml: JobToml = toml::from_str(&content)
            .with_context(|| format!("failed to parse job.toml: {}", path.display()))?;
        job_toml.into_def()
    }

    fn write_job_sync(&self, workspace_id: &str, job: &JobDefinition) -> Result<()> {
        let job_dir = self.job_dir(workspace_id, &job.id);
        std::fs::create_dir_all(&job_dir)
            .with_context(|| format!("failed to create job dir: {}", job_dir.display()))?;
        // Ensure runs/ subdirectory exists
        let runs_dir = job_dir.join("runs");
        std::fs::create_dir_all(&runs_dir).ok();

        let job_toml = JobToml::from_def(job);
        let content = toml::to_string_pretty(&job_toml)
            .context("failed to serialize job.toml")?;
        let path = self.job_toml_path(workspace_id, &job.id);
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write job.toml: {}", path.display()))?;
        Ok(())
    }
}

#[async_trait]
impl SchedulerStore for FsSchedulerStore {
    async fn create_job(&self, workspace_id: &str, job: &JobDefinition) -> Result<JobDefinition> {
        // Create the jobs/ parent dir if it doesn't exist yet
        let jobs_dir = self.jobs_dir(workspace_id);
        tokio::fs::create_dir_all(&jobs_dir).await?;

        self.write_job_sync(workspace_id, job)?;
        info!(workspace = workspace_id, job = %job.id, name = %job.name, "created job");
        Ok(job.clone())
    }

    async fn get_job(&self, workspace_id: &str, job_id: &str) -> Result<JobDefinition> {
        self.read_job_sync(workspace_id, job_id)
    }

    async fn list_jobs(&self, workspace_id: &str) -> Result<Vec<JobDefinition>> {
        let jobs_dir = self.jobs_dir(workspace_id);
        if !jobs_dir.exists() {
            return Ok(vec![]);
        }

        let mut jobs = Vec::new();
        let mut entries = tokio::fs::read_dir(&jobs_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let job_id = match path.file_name().and_then(|n| n.to_str()) {
                Some(id) => id.to_string(),
                None => continue,
            };
            match self.read_job_sync(workspace_id, &job_id) {
                Ok(job) => jobs.push(job),
                Err(e) => {
                    debug!(workspace = workspace_id, job = %job_id, err = %e, "skipping corrupt job.toml");
                }
            }
        }
        Ok(jobs)
    }

    async fn update_job(&self, workspace_id: &str, job: &JobDefinition) -> Result<JobDefinition> {
        self.write_job_sync(workspace_id, job)?;
        Ok(job.clone())
    }

    async fn delete_job(&self, workspace_id: &str, job_id: &str) -> Result<()> {
        let job_dir = self.job_dir(workspace_id, job_id);
        if job_dir.exists() {
            tokio::fs::remove_dir_all(&job_dir)
                .await
                .with_context(|| format!("failed to delete job dir: {}", job_dir.display()))?;
            info!(workspace = workspace_id, job = %job_id, "deleted job");
        }
        Ok(())
    }

    async fn write_run(&self, workspace_id: &str, job_id: &str, run: &JobRun) -> Result<()> {
        let runs_dir = self.runs_dir(workspace_id, job_id);
        tokio::fs::create_dir_all(&runs_dir).await?;

        // Write JSON run record
        let run_path = self.run_path(workspace_id, job_id, &run.timestamp);
        let json = serde_json::to_string_pretty(run)
            .context("failed to serialize job run")?;
        tokio::fs::write(&run_path, json)
            .await
            .with_context(|| format!("failed to write run: {}", run_path.display()))?;

        // Update output.md with the latest successful output
        if !run.output.is_empty() {
            let output_path = self.output_md_path(workspace_id, job_id);
            tokio::fs::write(&output_path, &run.output).await?;
        }

        debug!(workspace = workspace_id, job = %job_id, status = %run.status, "wrote run");
        Ok(())
    }

    async fn list_runs(
        &self,
        workspace_id: &str,
        job_id: &str,
        limit: usize,
    ) -> Result<Vec<JobRun>> {
        let runs_dir = self.runs_dir(workspace_id, job_id);
        if !runs_dir.exists() {
            return Ok(vec![]);
        }

        // Collect all .json files
        let mut file_names: Vec<String> = Vec::new();
        let mut entries = tokio::fs::read_dir(&runs_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if name_str.ends_with(".json") {
                file_names.push(name_str);
            }
        }

        // Sort descending (most recent first) — filenames are timestamp-based
        file_names.sort_by(|a, b| b.cmp(a));

        let mut runs = Vec::new();
        for name in file_names.iter().take(limit) {
            let path = runs_dir.join(name);
            match tokio::fs::read_to_string(&path).await {
                Ok(content) => match serde_json::from_str::<JobRun>(&content) {
                    Ok(run) => runs.push(run),
                    Err(e) => {
                        debug!(path = %path.display(), err = %e, "skipping corrupt run file");
                    }
                },
                Err(e) => {
                    debug!(path = %path.display(), err = %e, "failed to read run file");
                }
            }
        }
        Ok(runs)
    }

    async fn prune_runs(
        &self,
        workspace_id: &str,
        job_id: &str,
        max_history: usize,
    ) -> Result<u64> {
        if max_history == 0 {
            return Ok(0); // Keep all
        }

        let runs_dir = self.runs_dir(workspace_id, job_id);
        if !runs_dir.exists() {
            return Ok(0);
        }

        let mut file_names: Vec<String> = Vec::new();
        let mut entries = tokio::fs::read_dir(&runs_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name_str = name.to_string_lossy().to_string();
            if name_str.ends_with(".json") {
                file_names.push(name_str);
            }
        }

        // Sort descending to keep most recent first
        file_names.sort_by(|a, b| b.cmp(a));

        let mut removed = 0u64;
        for name in file_names.iter().skip(max_history) {
            let path = runs_dir.join(name);
            if let Err(e) = tokio::fs::remove_file(&path).await {
                debug!(path = %path.display(), err = %e, "failed to remove old run");
            } else {
                removed += 1;
            }
        }

        if removed > 0 {
            info!(workspace = workspace_id, job = %job_id, removed, "pruned old runs");
        }
        Ok(removed)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::scheduler::JobStatus;
    use uuid::Uuid;

    fn make_job(id: &str) -> JobDefinition {
        let now = Utc::now();
        JobDefinition {
            id: id.to_string(),
            name: "Test Job".to_string(),
            prompt: "Do something".to_string(),
            schedule: "0 9 * * *".to_string(),
            timezone: None,
            enabled: true,
            one_shot: false,
            notify: true,
            max_history: 7,
            created_at: now,
            updated_at: now,
            last_run_at: None,
        }
    }

    fn make_run(job_id: &str, status: JobStatus) -> JobRun {
        JobRun {
            job_id: job_id.to_string(),
            timestamp: Utc::now(),
            status,
            output: "Some output".to_string(),
            duration_ms: 1234,
            error_message: None,
        }
    }

    #[tokio::test]
    async fn create_and_get_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        // Create the workspace dir first (normally done by FsWorkspaceStore)
        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        let created = store.create_job("ws-1", &job).await.unwrap();
        assert_eq!(created.id, job.id);

        let loaded = store.get_job("ws-1", &job.id).await.unwrap();
        assert_eq!(loaded.name, "Test Job");
        assert_eq!(loaded.schedule, "0 9 * * *");
    }

    #[tokio::test]
    async fn list_jobs_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();
        let jobs = store.list_jobs("ws-1").await.unwrap();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn list_multiple_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job1 = make_job(&Uuid::new_v4().to_string());
        let job2 = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job1).await.unwrap();
        store.create_job("ws-1", &job2).await.unwrap();

        let jobs = store.list_jobs("ws-1").await.unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[tokio::test]
    async fn update_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let mut job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();

        job.enabled = false;
        job.name = "Updated Name".to_string();
        store.update_job("ws-1", &job).await.unwrap();

        let loaded = store.get_job("ws-1", &job.id).await.unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.name, "Updated Name");
    }

    #[tokio::test]
    async fn delete_job() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();
        store.delete_job("ws-1", &job.id).await.unwrap();

        assert!(store.get_job("ws-1", &job.id).await.is_err());
    }

    #[tokio::test]
    async fn write_and_list_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();

        let run1 = make_run(&job.id, JobStatus::Success);
        store.write_run("ws-1", &job.id, &run1).await.unwrap();

        let run2 = make_run(&job.id, JobStatus::Error);
        store.write_run("ws-1", &job.id, &run2).await.unwrap();

        let runs = store.list_runs("ws-1", &job.id, 10).await.unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[tokio::test]
    async fn prune_runs() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();

        // Write 5 runs with slight delays so filenames differ
        for _ in 0..5 {
            // Use a slightly different timestamp each time
            let run = JobRun {
                job_id: job.id.clone(),
                timestamp: Utc::now(),
                status: JobStatus::Success,
                output: "output".to_string(),
                duration_ms: 100,
                error_message: None,
            };
            store.write_run("ws-1", &job.id, &run).await.unwrap();
            // Small sleep so timestamps differ
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        let removed = store.prune_runs("ws-1", &job.id, 3).await.unwrap();
        assert_eq!(removed, 2);

        let runs = store.list_runs("ws-1", &job.id, 100).await.unwrap();
        assert_eq!(runs.len(), 3);
    }

    #[tokio::test]
    async fn prune_zero_max_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();

        for _ in 0..3 {
            let run = make_run(&job.id, JobStatus::Success);
            store.write_run("ws-1", &job.id, &run).await.unwrap();
        }

        let removed = store.prune_runs("ws-1", &job.id, 0).await.unwrap();
        assert_eq!(removed, 0);

        let runs = store.list_runs("ws-1", &job.id, 100).await.unwrap();
        assert_eq!(runs.len(), 3);
    }

    #[tokio::test]
    async fn output_md_written_on_run() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsSchedulerStore::new(dir.path()).unwrap();

        tokio::fs::create_dir_all(dir.path().join("workspaces").join("ws-1"))
            .await
            .unwrap();

        let job = make_job(&Uuid::new_v4().to_string());
        store.create_job("ws-1", &job).await.unwrap();

        let run = JobRun {
            job_id: job.id.clone(),
            timestamp: Utc::now(),
            status: JobStatus::Success,
            output: "# Summary\n\nSome useful content".to_string(),
            duration_ms: 500,
            error_message: None,
        };
        store.write_run("ws-1", &job.id, &run).await.unwrap();

        let output_path = store.output_md_path("ws-1", &job.id);
        let content = tokio::fs::read_to_string(&output_path).await.unwrap();
        assert_eq!(content, "# Summary\n\nSome useful content");
    }
}
