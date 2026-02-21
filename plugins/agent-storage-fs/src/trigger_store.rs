//! Filesystem implementation of `TriggerStore`.
//!
//! Trigger folder layout under `<data_dir>/workspaces/<workspace_id>/triggers/`:
//!
//! ```text
//! triggers/
//! ├── <trigger-uuid>/
//! │   ├── trigger.toml     # TriggerDefinition (TOML)
//! │   ├── output.md        # Latest run output (plain text)
//! │   └── runs/
//! │       ├── 2026-02-15T14-32-01Z.json   # TriggerRun records (JSON)
//! │       └── ...
//! └── ...
//! ```
//!
//! Token resolution uses an in-memory index rebuilt lazily on first call
//! and invalidated on any CRUD operation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info};

use agent_core::trigger::{
    generate_token, TriggerDefinition, TriggerRun, TriggerStore,
};

// ---------------------------------------------------------------------------
// On-disk TOML representation of TriggerDefinition
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct TriggerToml {
    id: String,
    name: String,
    prompt_template: String,
    token: String,
    enabled: bool,
    notify: bool,
    max_history: u32,
    rate_limit_per_minute: u32,
    #[serde(default)]
    hmac_secret: Option<String>,
    #[serde(default)]
    accepted_content_types: Vec<String>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    last_event_at: Option<String>,
}

impl TriggerToml {
    fn from_def(t: &TriggerDefinition) -> Self {
        Self {
            id: t.id.clone(),
            name: t.name.clone(),
            prompt_template: t.prompt_template.clone(),
            token: t.token.clone(),
            enabled: t.enabled,
            notify: t.notify,
            max_history: t.max_history,
            rate_limit_per_minute: t.rate_limit_per_minute,
            hmac_secret: t.hmac_secret.clone(),
            accepted_content_types: t.accepted_content_types.clone(),
            created_at: t.created_at.to_rfc3339(),
            updated_at: t.updated_at.to_rfc3339(),
            last_event_at: t.last_event_at.map(|dt| dt.to_rfc3339()),
        }
    }

    fn into_def(self) -> Result<TriggerDefinition> {
        let parse = |s: &str| -> Result<DateTime<Utc>> {
            DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&Utc))
                .with_context(|| format!("invalid datetime: {}", s))
        };
        Ok(TriggerDefinition {
            id: self.id,
            name: self.name,
            prompt_template: self.prompt_template,
            token: self.token,
            enabled: self.enabled,
            notify: self.notify,
            max_history: self.max_history,
            rate_limit_per_minute: self.rate_limit_per_minute,
            hmac_secret: self.hmac_secret,
            accepted_content_types: self.accepted_content_types,
            created_at: parse(&self.created_at)?,
            updated_at: parse(&self.updated_at)?,
            last_event_at: self.last_event_at.as_deref().map(parse).transpose()?,
        })
    }
}

// ---------------------------------------------------------------------------
// FsTriggerStore
// ---------------------------------------------------------------------------

/// Filesystem-backed trigger store.
///
/// The `token_index` maps `token → (workspace_id, trigger_id)` and is built
/// lazily on first `resolve_token` call. It is invalidated (cleared) on every
/// CRUD mutation so the next resolution rescans the disk.
pub struct FsTriggerStore {
    data_dir: PathBuf,
    /// In-memory token index: token → (workspace_id, TriggerDefinition)
    token_index: Arc<RwLock<Option<HashMap<String, (String, TriggerDefinition)>>>>,
}

impl FsTriggerStore {
    /// Create a new store rooted at `data_dir`.
    /// The directory must already exist (created by `FsWorkspaceStore`).
    pub fn new(data_dir: &Path) -> Result<Self> {
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            token_index: Arc::new(RwLock::new(None)),
        })
    }

    // --- Path helpers -------------------------------------------------------

    fn workspace_triggers_dir(&self, workspace_id: &str) -> PathBuf {
        self.data_dir
            .join("workspaces")
            .join(workspace_id)
            .join("triggers")
    }

    fn trigger_dir(&self, workspace_id: &str, trigger_id: &str) -> PathBuf {
        self.workspace_triggers_dir(workspace_id).join(trigger_id)
    }

    fn trigger_toml_path(&self, workspace_id: &str, trigger_id: &str) -> PathBuf {
        self.trigger_dir(workspace_id, trigger_id).join("trigger.toml")
    }

    fn runs_dir(&self, workspace_id: &str, trigger_id: &str) -> PathBuf {
        self.trigger_dir(workspace_id, trigger_id).join("runs")
    }

    fn output_md_path(&self, workspace_id: &str, trigger_id: &str) -> PathBuf {
        self.trigger_dir(workspace_id, trigger_id).join("output.md")
    }

    // --- Low-level helpers --------------------------------------------------

    fn read_trigger_toml(&self, workspace_id: &str, trigger_id: &str) -> Result<TriggerDefinition> {
        let path = self.trigger_toml_path(workspace_id, trigger_id);
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let toml_val: TriggerToml = toml::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        toml_val.into_def()
    }

    fn write_trigger_toml(&self, workspace_id: &str, trigger: &TriggerDefinition) -> Result<()> {
        let dir = self.trigger_dir(workspace_id, &trigger.id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create trigger dir {}", dir.display()))?;
        let path = self.trigger_toml_path(workspace_id, &trigger.id);
        let text = toml::to_string(&TriggerToml::from_def(trigger))
            .context("failed to serialize trigger.toml")?;
        std::fs::write(&path, text)
            .with_context(|| format!("failed to write {}", path.display()))
    }

    /// Invalidate the in-memory token index (called after any CRUD mutation).
    async fn invalidate_index(&self) {
        let mut idx = self.token_index.write().await;
        *idx = None;
    }

    /// Build (or return cached) token index by scanning all workspaces.
    async fn ensure_index(&self) -> Result<()> {
        // Fast path: already built
        {
            let idx = self.token_index.read().await;
            if idx.is_some() {
                return Ok(());
            }
        }

        // Build index
        let mut map: HashMap<String, (String, TriggerDefinition)> = HashMap::new();

        let workspaces_dir = self.data_dir.join("workspaces");
        if !workspaces_dir.exists() {
            let mut idx = self.token_index.write().await;
            *idx = Some(map);
            return Ok(());
        }

        let entries = std::fs::read_dir(&workspaces_dir)
            .context("failed to read workspaces directory")?;

        for ws_entry in entries.flatten() {
            let ws_id = ws_entry.file_name().to_string_lossy().into_owned();
            let triggers_dir = ws_entry.path().join("triggers");
            if !triggers_dir.is_dir() {
                continue;
            }

            let trigger_entries = match std::fs::read_dir(&triggers_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for t_entry in trigger_entries.flatten() {
                let trigger_id = t_entry.file_name().to_string_lossy().into_owned();
                match self.read_trigger_toml(&ws_id, &trigger_id) {
                    Ok(trigger) => {
                        map.insert(trigger.token.clone(), (ws_id.clone(), trigger));
                    }
                    Err(e) => {
                        debug!(workspace = %ws_id, trigger = %trigger_id, err = %e, "skipping corrupt trigger.toml");
                    }
                }
            }
        }

        let mut idx = self.token_index.write().await;
        *idx = Some(map);
        Ok(())
    }

    // --- Run file helpers ---------------------------------------------------

    fn timestamp_to_filename(ts: &DateTime<Utc>) -> String {
        ts.to_rfc3339().replace(':', "-")
    }

    #[allow(dead_code)]
    fn filename_to_timestamp(name: &str) -> Result<DateTime<Utc>> {
        let s = name.trim_end_matches(".json").replace('-', ":");
        // RFC3339: 2026-02-15T14:32:01+00:00 — restore the colons we swapped
        // The filename is "2026-02-15T14-32-01+00-00.json"; we replace '-' with ':'
        // which overshoots the date separators, so use a heuristic:
        // actually store as "2026-02-15T14:32:01Z" → filename "2026-02-15T14-32-01Z"
        // The T stays, date separators stay, only time colons become dashes.
        // Simpler: just try to parse directly.
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&Utc))
            .with_context(|| format!("invalid run filename: {}", name))
    }

    fn run_file_path(&self, workspace_id: &str, trigger_id: &str, run: &TriggerRun) -> PathBuf {
        let filename = format!("{}.json", Self::timestamp_to_filename(&run.timestamp));
        self.runs_dir(workspace_id, trigger_id).join(filename)
    }
}

#[async_trait]
impl TriggerStore for FsTriggerStore {
    async fn create_trigger(
        &self,
        workspace_id: &str,
        trigger: &TriggerDefinition,
    ) -> Result<TriggerDefinition> {
        self.write_trigger_toml(workspace_id, trigger)?;
        self.invalidate_index().await;
        info!(workspace = %workspace_id, trigger = %trigger.id, name = %trigger.name, "trigger created");
        Ok(trigger.clone())
    }

    async fn get_trigger(
        &self,
        workspace_id: &str,
        trigger_id: &str,
    ) -> Result<TriggerDefinition> {
        self.read_trigger_toml(workspace_id, trigger_id)
            .with_context(|| format!("trigger '{}' not found in workspace '{}'", trigger_id, workspace_id))
    }

    async fn list_triggers(&self, workspace_id: &str) -> Result<Vec<TriggerDefinition>> {
        let dir = self.workspace_triggers_dir(workspace_id);
        if !dir.exists() {
            return Ok(vec![]);
        }

        let mut triggers = Vec::new();
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("failed to read triggers dir for workspace '{}'", workspace_id))?;

        for entry in entries.flatten() {
            let trigger_id = entry.file_name().to_string_lossy().into_owned();
            match self.read_trigger_toml(workspace_id, &trigger_id) {
                Ok(t) => triggers.push(t),
                Err(e) => {
                    debug!(workspace = %workspace_id, trigger = %trigger_id, err = %e, "skipping corrupt trigger");
                }
            }
        }

        Ok(triggers)
    }

    async fn update_trigger(
        &self,
        workspace_id: &str,
        trigger: &TriggerDefinition,
    ) -> Result<TriggerDefinition> {
        self.write_trigger_toml(workspace_id, trigger)?;
        self.invalidate_index().await;
        debug!(workspace = %workspace_id, trigger = %trigger.id, "trigger updated");
        Ok(trigger.clone())
    }

    async fn delete_trigger(&self, workspace_id: &str, trigger_id: &str) -> Result<()> {
        let dir = self.trigger_dir(workspace_id, trigger_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("failed to delete trigger dir {}", dir.display()))?;
        }
        self.invalidate_index().await;
        info!(workspace = %workspace_id, trigger = %trigger_id, "trigger deleted");
        Ok(())
    }

    async fn resolve_token(
        &self,
        token: &str,
    ) -> Result<Option<(String, TriggerDefinition)>> {
        self.ensure_index().await?;
        let idx = self.token_index.read().await;
        Ok(idx
            .as_ref()
            .and_then(|map| map.get(token))
            .filter(|(_, t)| t.enabled)
            .map(|(ws_id, t)| (ws_id.clone(), t.clone())))
    }

    async fn write_run(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        run: &TriggerRun,
    ) -> Result<()> {
        let runs_dir = self.runs_dir(workspace_id, trigger_id);
        std::fs::create_dir_all(&runs_dir)
            .with_context(|| format!("failed to create runs dir {}", runs_dir.display()))?;

        // Write JSON run record
        let path = self.run_file_path(workspace_id, trigger_id, run);
        let json = serde_json::to_string_pretty(run).context("failed to serialize run")?;
        std::fs::write(&path, json)
            .with_context(|| format!("failed to write run file {}", path.display()))?;

        // Update output.md with latest non-empty output
        if !run.output.is_empty() {
            let output_path = self.output_md_path(workspace_id, trigger_id);
            std::fs::write(&output_path, &run.output)
                .with_context(|| format!("failed to write {}", output_path.display()))?;
        }

        debug!(workspace = %workspace_id, trigger = %trigger_id, status = %run.status, "run written");
        Ok(())
    }

    async fn list_runs(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        limit: usize,
    ) -> Result<Vec<TriggerRun>> {
        let runs_dir = self.runs_dir(workspace_id, trigger_id);
        if !runs_dir.exists() {
            return Ok(vec![]);
        }

        let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)
            .context("failed to read runs directory")?
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".json")
            })
            .collect();

        // Sort by filename descending (most recent first)
        entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        let mut runs = Vec::new();
        for entry in entries.into_iter().take(limit) {
            let path = entry.path();
            match std::fs::read_to_string(&path)
                .and_then(|s| Ok(s))
                .and_then(|s| serde_json::from_str::<TriggerRun>(&s).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
            {
                Ok(run) => runs.push(run),
                Err(e) => {
                    debug!(path = %path.display(), err = %e, "skipping corrupt run file");
                }
            }
        }

        Ok(runs)
    }

    async fn prune_runs(
        &self,
        workspace_id: &str,
        trigger_id: &str,
        max_history: usize,
    ) -> Result<u64> {
        if max_history == 0 {
            return Ok(0);
        }

        let runs_dir = self.runs_dir(workspace_id, trigger_id);
        if !runs_dir.exists() {
            return Ok(0);
        }

        let mut entries: Vec<_> = std::fs::read_dir(&runs_dir)
            .context("failed to read runs directory")?
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
            .collect();

        if entries.len() <= max_history {
            return Ok(0);
        }

        // Sort oldest first (ascending)
        entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

        let to_delete = entries.len() - max_history;
        let mut deleted = 0u64;
        for entry in entries.into_iter().take(to_delete) {
            if std::fs::remove_file(entry.path()).is_ok() {
                deleted += 1;
            }
        }

        debug!(workspace = %workspace_id, trigger = %trigger_id, deleted, "pruned old runs");
        Ok(deleted)
    }

    async fn regenerate_token(
        &self,
        workspace_id: &str,
        trigger_id: &str,
    ) -> Result<TriggerDefinition> {
        let mut trigger = self.read_trigger_toml(workspace_id, trigger_id)
            .with_context(|| format!("trigger '{}' not found", trigger_id))?;

        trigger.token = generate_token();
        trigger.updated_at = Utc::now();

        self.write_trigger_toml(workspace_id, &trigger)?;
        self.invalidate_index().await;

        info!(workspace = %workspace_id, trigger = %trigger_id, "trigger token regenerated");
        Ok(trigger)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::trigger::TriggerStatus;
    use tempfile::TempDir;

    fn make_store(dir: &TempDir) -> FsTriggerStore {
        FsTriggerStore::new(dir.path()).unwrap()
    }

    fn sample_trigger() -> TriggerDefinition {
        let now = Utc::now();
        TriggerDefinition {
            id: uuid::Uuid::new_v4().to_string(),
            name: "Test Trigger".to_string(),
            prompt_template: "Process: {{payload}}".to_string(),
            token: generate_token(),
            enabled: true,
            notify: true,
            max_history: 5,
            rate_limit_per_minute: 10,
            hmac_secret: None,
            accepted_content_types: vec![],
            created_at: now,
            updated_at: now,
            last_event_at: None,
        }
    }

    fn sample_run(trigger_id: &str) -> TriggerRun {
        TriggerRun {
            trigger_id: trigger_id.to_string(),
            timestamp: Utc::now(),
            status: TriggerStatus::Success,
            payload_preview: "test payload".to_string(),
            output: "LLM response".to_string(),
            duration_ms: 1500,
            error_message: None,
            source_ip: None,
        }
    }

    #[tokio::test]
    async fn create_and_get() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let trigger = sample_trigger();
        let ws = "ws1";

        let created = store.create_trigger(ws, &trigger).await.unwrap();
        assert_eq!(created.id, trigger.id);

        let loaded = store.get_trigger(ws, &trigger.id).await.unwrap();
        assert_eq!(loaded.name, "Test Trigger");
        assert_eq!(loaded.token, trigger.token);
    }

    #[tokio::test]
    async fn list_triggers() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";

        let t1 = sample_trigger();
        let mut t2 = sample_trigger();
        t2.name = "Second".to_string();

        store.create_trigger(ws, &t1).await.unwrap();
        store.create_trigger(ws, &t2).await.unwrap();

        let list = store.list_triggers(ws).await.unwrap();
        assert_eq!(list.len(), 2);
    }

    #[tokio::test]
    async fn update_trigger() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let mut trigger = sample_trigger();

        store.create_trigger(ws, &trigger).await.unwrap();

        trigger.name = "Updated Name".to_string();
        trigger.enabled = false;
        store.update_trigger(ws, &trigger).await.unwrap();

        let loaded = store.get_trigger(ws, &trigger.id).await.unwrap();
        assert_eq!(loaded.name, "Updated Name");
        assert!(!loaded.enabled);
    }

    #[tokio::test]
    async fn delete_trigger() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();

        store.create_trigger(ws, &trigger).await.unwrap();
        store.delete_trigger(ws, &trigger.id).await.unwrap();

        assert!(store.get_trigger(ws, &trigger.id).await.is_err());
    }

    #[tokio::test]
    async fn resolve_token() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();
        let token = trigger.token.clone();

        store.create_trigger(ws, &trigger).await.unwrap();

        let result = store.resolve_token(&token).await.unwrap();
        assert!(result.is_some());
        let (found_ws, found_trigger) = result.unwrap();
        assert_eq!(found_ws, ws);
        assert_eq!(found_trigger.id, trigger.id);
    }

    #[tokio::test]
    async fn resolve_unknown_token() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let result = store.resolve_token("wh_unknown").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn resolve_disabled_trigger_returns_none() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let mut trigger = sample_trigger();
        trigger.enabled = false;
        let token = trigger.token.clone();

        store.create_trigger(ws, &trigger).await.unwrap();

        let result = store.resolve_token(&token).await.unwrap();
        assert!(result.is_none(), "disabled trigger should not resolve");
    }

    #[tokio::test]
    async fn write_and_list_runs() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();
        store.create_trigger(ws, &trigger).await.unwrap();

        let run = sample_run(&trigger.id);
        store.write_run(ws, &trigger.id, &run).await.unwrap();

        let runs = store.list_runs(ws, &trigger.id, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].output, "LLM response");
    }

    #[tokio::test]
    async fn prune_runs() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();
        store.create_trigger(ws, &trigger).await.unwrap();

        for _ in 0..5 {
            let mut run = sample_run(&trigger.id);
            run.timestamp = Utc::now();
            store.write_run(ws, &trigger.id, &run).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        }

        let deleted = store.prune_runs(ws, &trigger.id, 3).await.unwrap();
        assert_eq!(deleted, 2);

        let remaining = store.list_runs(ws, &trigger.id, 10).await.unwrap();
        assert_eq!(remaining.len(), 3);
    }

    #[tokio::test]
    async fn regenerate_token() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();
        let old_token = trigger.token.clone();

        store.create_trigger(ws, &trigger).await.unwrap();

        let updated = store.regenerate_token(ws, &trigger.id).await.unwrap();
        assert_ne!(updated.token, old_token);
        assert!(updated.token.starts_with("wh_"));

        // Old token no longer resolves
        let old_result = store.resolve_token(&old_token).await.unwrap();
        assert!(old_result.is_none());

        // New token resolves
        let new_result = store.resolve_token(&updated.token).await.unwrap();
        assert!(new_result.is_some());
    }

    #[tokio::test]
    async fn index_invalidated_after_delete() {
        let dir = TempDir::new().unwrap();
        let store = make_store(&dir);
        let ws = "ws1";
        let trigger = sample_trigger();
        let token = trigger.token.clone();

        store.create_trigger(ws, &trigger).await.unwrap();
        // Prime the index
        let _ = store.resolve_token(&token).await.unwrap();

        store.delete_trigger(ws, &trigger.id).await.unwrap();

        // After delete, index is invalidated and token should not resolve
        let result = store.resolve_token(&token).await.unwrap();
        assert!(result.is_none());
    }
}
