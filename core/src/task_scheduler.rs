//! Scheduled task management — persistence, scheduling, and execution.
//!
//! Tasks are JSON files stored in `.aptove/tasks/`. Each task runs either once
//! at a specific time or on a recurring schedule (daily / weekly / monthly).
//! The scheduler wakes every 60 s, finds due tasks, and sends them to the agent
//! as regular `session/new` + `session/prompt` sequences.

use chrono::{Datelike, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub schedule: TaskSchedule,
    pub enabled: bool,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TaskSchedule {
    #[serde(rename_all = "camelCase")]
    Once {
        /// ISO 8601 datetime string
        at: String,
    },
    #[serde(rename_all = "camelCase")]
    Recurring {
        frequency: Frequency,
        /// "HH:MM" in the given timezone
        time: String,
        /// 0=Sun..6=Sat (for Weekly)
        #[serde(skip_serializing_if = "Option::is_none")]
        day_of_week: Option<u8>,
        /// 1-31 (for Monthly)
        #[serde(skip_serializing_if = "Option::is_none")]
        day_of_month: Option<u8>,
        /// IANA timezone e.g. "America/New_York"
        timezone: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Load all tasks from the given directory.
pub fn load_tasks(dir: &Path) -> Vec<ScheduledTask> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("Failed to read tasks directory {}: {}", dir.display(), e);
            return Vec::new();
        }
    };
    let mut tasks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<ScheduledTask>(&contents) {
                Ok(task) => tasks.push(task),
                Err(e) => warn!("Invalid task file {}: {}", path.display(), e),
            },
            Err(e) => warn!("Failed to read task file {}: {}", path.display(), e),
        }
    }
    tasks
}

/// Async variant of load_tasks.
pub async fn load_tasks_async(dir: &Path) -> Vec<ScheduledTask> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || load_tasks(&dir))
        .await
        .unwrap_or_default()
}

/// Save a task to disk (atomic write via temp file + rename).
pub fn save_task(dir: &Path, task: &ScheduledTask) -> std::io::Result<()> {
    let target = dir.join(format!("{}.json", task.id));
    let tmp = dir.join(format!(".{}.json.tmp", task.id));
    let contents = serde_json::to_string_pretty(task)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, &target)?;
    Ok(())
}

/// Async variant of save_task.
pub async fn save_task_async(dir: &Path, task: &ScheduledTask) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    let task = task.clone();
    tokio::task::spawn_blocking(move || save_task(&dir, &task))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}

/// Delete a task file.
pub fn delete_task(dir: &Path, id: &str) -> std::io::Result<()> {
    let path = dir.join(format!("{}.json", id));
    std::fs::remove_file(path)
}

/// Async variant of delete_task.
pub async fn delete_task_async(dir: &Path, id: &str) -> std::io::Result<()> {
    let dir = dir.to_path_buf();
    let id = id.to_string();
    tokio::task::spawn_blocking(move || delete_task(&dir, &id))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?
}

// ---------------------------------------------------------------------------
// Next-run computation
// ---------------------------------------------------------------------------

/// Compute the next run time for a task. Returns ISO 8601 UTC string.
/// Returns `None` for one-time tasks that are in the past.
pub fn compute_next_run(task: &ScheduledTask) -> Option<String> {
    let now = Utc::now();
    match &task.schedule {
        TaskSchedule::Once { at } => {
            let dt = chrono::DateTime::parse_from_rfc3339(at).ok()?;
            if dt > now {
                Some(dt.to_rfc3339())
            } else {
                None // already past
            }
        }
        TaskSchedule::Recurring {
            frequency,
            time,
            day_of_week,
            day_of_month,
            timezone,
        } => {
            let tz: Tz = timezone.parse().ok()?;
            let parts: Vec<&str> = time.split(':').collect();
            if parts.len() != 2 {
                return None;
            }
            let hour: u32 = parts[0].parse().ok()?;
            let minute: u32 = parts[1].parse().ok()?;
            let target_time = NaiveTime::from_hms_opt(hour, minute, 0)?;

            let now_local = now.with_timezone(&tz);
            let today = now_local.date_naive();

            match frequency {
                Frequency::Daily => {
                    // Try today at target_time, if past try tomorrow
                    let candidate = today.and_time(target_time);
                    let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;
                    if candidate_tz > now_local {
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    } else {
                        let tomorrow = today.succ_opt()?;
                        let candidate = tomorrow.and_time(target_time);
                        let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    }
                }
                Frequency::Weekly => {
                    let target_dow = day_of_week.unwrap_or(1) as u32; // default Monday
                    // chrono: Mon=1..Sun=7; our encoding: Sun=0..Sat=6
                    let chrono_dow = if target_dow == 0 { 7 } else { target_dow };
                    let current_dow = now_local.weekday().num_days_from_monday() + 1; // 1=Mon..7=Sun

                    let days_ahead = (chrono_dow as i64 - current_dow as i64).rem_euclid(7);
                    let candidate_date = today + chrono::Duration::days(days_ahead);
                    let candidate = candidate_date.and_time(target_time);
                    let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;

                    if candidate_tz > now_local {
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    } else {
                        // Same day but time already passed, advance by 7 days
                        let next_date = candidate_date + chrono::Duration::days(7);
                        let candidate = next_date.and_time(target_time);
                        let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    }
                }
                Frequency::Monthly => {
                    let target_day = day_of_month.unwrap_or(1).max(1).min(28) as u32;
                    let this_month = chrono::NaiveDate::from_ymd_opt(
                        today.year(),
                        today.month(),
                        target_day.min(days_in_month(today.year(), today.month())),
                    )?;
                    let candidate = this_month.and_time(target_time);
                    let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;

                    if candidate_tz > now_local {
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    } else {
                        // Next month
                        let (next_year, next_month) = if today.month() == 12 {
                            (today.year() + 1, 1)
                        } else {
                            (today.year(), today.month() + 1)
                        };
                        let next_date = chrono::NaiveDate::from_ymd_opt(
                            next_year,
                            next_month,
                            target_day.min(days_in_month(next_year, next_month)),
                        )?;
                        let candidate = next_date.and_time(target_time);
                        let candidate_tz = tz.from_local_datetime(&candidate).earliest()?;
                        Some(candidate_tz.with_timezone(&Utc).to_rfc3339())
                    }
                }
            }
        }
    }
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

// ---------------------------------------------------------------------------
// Default tasks
// ---------------------------------------------------------------------------

const DEFAULT_COMPACT_MEMORY_NAME: &str = "Compact Memory";

const DEFAULT_COMPACT_MEMORY_PROMPT: &str = "\
[SYSTEM: Memory Compaction]\n\
Read the current <memory> content. Consolidate, deduplicate, remove outdated entries, \
and resolve contradictions. Keep the content well-organized with clear sections. \
Respond with ONLY the cleaned content wrapped in exactly these XML tags — no other text before or after:\n\n\
<merged_memory>\n\
(consolidated content here)\n\
</merged_memory>";

/// Ensure the default "Compact Memory" task exists. Creates it if missing.
///
/// **Missed-run handling:** When the bridge restarts after being down, tasks whose
/// `next_run_at` is in the past will fire once on the first scheduler tick. For
/// recurring tasks, `compute_next_run` then advances to the next future slot —
/// so only one catch-up execution happens per task, not a backlog of missed runs.
/// One-time past tasks execute once and their file is deleted.
pub fn ensure_default_tasks(dir: &Path) {
    let tasks = load_tasks(dir);
    let has_compact = tasks.iter().any(|t| t.name == DEFAULT_COMPACT_MEMORY_NAME);
    if !has_compact {
        info!("Creating default '{}' task", DEFAULT_COMPACT_MEMORY_NAME);
        let tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
        let task = ScheduledTask {
            id: uuid::Uuid::new_v4().to_string(),
            name: DEFAULT_COMPACT_MEMORY_NAME.to_string(),
            prompt: DEFAULT_COMPACT_MEMORY_PROMPT.to_string(),
            schedule: TaskSchedule::Recurring {
                frequency: Frequency::Daily,
                time: "04:00".to_string(),
                day_of_week: None,
                day_of_month: None,
                timezone: tz,
            },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        };
        let mut task_with_next = task;
        task_with_next.next_run_at = compute_next_run(&task_with_next);
        if let Err(e) = save_task(dir, &task_with_next) {
            error!("Failed to create default task: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

/// Start the background task scheduler. Wakes every 60 s to check for due tasks.
///
/// When a task is due, it sends a `session/new` + `session/prompt` sequence to the
/// agent via the pool's stdin channel. Responses flow through the normal agent_to_ws
/// broadcast path (buffered if no client is connected).
///
/// `inject_tx_provider` is called to get a sender for injecting `bridge/taskExecuted`
/// notifications into the client WebSocket stream. It may return `None` if no client
/// is connected (the notification is best-effort).
pub async fn start_scheduler(
    tasks_dir: PathBuf,
    pool: std::sync::Arc<tokio::sync::RwLock<crate::agent_pool::AgentPool>>,
    token: String,
    agent_command: String,
    cwd: String,
    memory_path: Option<PathBuf>,
    memory_max_bytes: u64,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let tasks = load_tasks_async(&tasks_dir).await;
            let now = Utc::now();

            for task in &tasks {
                if !task.enabled {
                    continue;
                }
                let next_run = match &task.next_run_at {
                    Some(nr) => nr.clone(),
                    None => continue,
                };
                let next_dt = match chrono::DateTime::parse_from_rfc3339(&next_run) {
                    Ok(dt) => dt.with_timezone(&Utc),
                    Err(_) => continue,
                };
                if next_dt > now {
                    continue;
                }

                info!("⏰ Executing scheduled task: {} ({})", task.name, task.id);

                // Get or spawn agent.
                // Capture agent_rx so we can subscribe to the broadcast channel before
                // sending session/new — this lets us await the actual response instead
                // of sleeping. It also prevents session/new protocol messages from going
                // into the overflow buffer (they are internal, not content for the mobile).
                // The subscription is dropped after we obtain the session ID, so the
                // subsequent session/prompt response goes to the overflow buffer and
                // triggers a push notification as normal.
                let (ws_to_agent_tx, agent_rx, _, _, _, _, _) = {
                    let mut pool_guard = pool.write().await;
                    match pool_guard.get_or_spawn(&token, &agent_command).await {
                        Ok(result) => result,
                        Err(e) => {
                            error!("⏰ Task '{}' failed: could not spawn agent: {}", task.name, e);
                            continue;
                        }
                    }
                };

                // Send session/new
                let session_new_id = format!("__task_session_new_{}", uuid::Uuid::new_v4().simple());
                let session_new = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": session_new_id,
                    "method": "session/new",
                    "params": {
                        "cwd": cwd,
                        "mcpServers": []
                    }
                });
                if let Err(e) = ws_to_agent_tx.send(session_new.to_string()).await {
                    error!("⏰ Task '{}' failed: could not send session/new: {}", task.name, e);
                    continue;
                }

                // Await the session/new response — no sleep, wait for the actual event.
                // Some agents (e.g. Claude) emit session/update notifications with the
                // sessionId before the final session/new result arrives, so we check both.
                let session_new_id_clone = session_new_id.clone();
                let session_id_result = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    async move {
                        use tokio::sync::broadcast::error::RecvError;
                        let mut agent_rx = agent_rx;
                        let mut found_session_id: Option<String> = None;
                        loop {
                            match agent_rx.recv().await {
                                Ok(msg) => {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg) {
                                        // session/update carries sessionId before the final result
                                        if v.get("method").and_then(|m| m.as_str()) == Some("session/update") {
                                            if let Some(sid) = v.pointer("/params/sessionId").and_then(|s| s.as_str()) {
                                                found_session_id = Some(sid.to_string());
                                            }
                                        }
                                        // Check if this is the response to our session/new request
                                        if v.get("id").and_then(|id| id.as_str()) == Some(session_new_id_clone.as_str()) {
                                            if v.get("error").is_some() {
                                                found_session_id = None;
                                            } else if let Some(sid) = v.pointer("/result/sessionId").and_then(|s| s.as_str()) {
                                                found_session_id = Some(sid.to_string());
                                            }
                                            break;
                                        }
                                    }
                                }
                                Err(RecvError::Closed) => break,
                                Err(RecvError::Lagged(_)) => continue,
                            }
                        }
                        found_session_id
                    }
                ).await;
                // agent_rx is dropped here (moved into async block above), so the
                // subscription is released before we send the prompt.

                let session_id = match session_id_result {
                    Err(_elapsed) => {
                        error!("⏰ Task '{}' failed: timed out waiting for session/new response", task.name);
                        if matches!(task.schedule, TaskSchedule::Once { .. }) {
                            if let Err(e) = delete_task_async(&tasks_dir, &task.id).await {
                                error!("⏰ Task '{}': also failed to delete task file: {}", task.name, e);
                            } else {
                                warn!("⏰ One-time task '{}' deleted after failure", task.name);
                            }
                        }
                        continue;
                    }
                    Ok(None) => {
                        error!("⏰ Task '{}' failed: agent rejected session/new", task.name);
                        if matches!(task.schedule, TaskSchedule::Once { .. }) {
                            if let Err(e) = delete_task_async(&tasks_dir, &task.id).await {
                                error!("⏰ Task '{}': also failed to delete task file: {}", task.name, e);
                            } else {
                                warn!("⏰ One-time task '{}' deleted after failure", task.name);
                            }
                        }
                        continue;
                    }
                    Ok(Some(id)) => id,
                };

                // Build the prompt
                let mut prompt_parts: Vec<serde_json::Value> = Vec::new();

                // Inject memory if available
                if let Some(ref mem_path) = memory_path {
                    let too_large = tokio::fs::metadata(mem_path)
                        .await
                        .map(|m| m.len() > memory_max_bytes)
                        .unwrap_or(false);
                    if !too_large {
                        if let Ok(contents) = tokio::fs::read_to_string(mem_path).await {
                            let trimmed = contents.trim();
                            if !trimmed.is_empty() {
                                prompt_parts.push(serde_json::json!({
                                    "type": "text",
                                    "text": format!("<memory>\n{}\n</memory>\n\n", trimmed)
                                }));
                            }
                        }
                    }
                }

                // Add the task prompt
                let prompt_text = format!("[Scheduled Task: {}]\n\n{}", task.name, task.prompt);
                prompt_parts.push(serde_json::json!({
                    "type": "text",
                    "text": prompt_text
                }));

                // Determine if this is the compact memory task — suppress its response
                let is_compact_memory = task.name == DEFAULT_COMPACT_MEMORY_NAME;
                let req_id = if is_compact_memory {
                    format!("__task_memory_compact_{}", uuid::Uuid::new_v4().simple())
                } else {
                    format!("__task_prompt_{}", uuid::Uuid::new_v4().simple())
                };

                if is_compact_memory {
                    let suppress_id = {
                        let pool_guard = pool.read().await;
                        pool_guard.get_suppress_response_id(&token)
                    };
                    if let Some(suppress_arc) = suppress_id {
                        if let Ok(mut guard) = suppress_arc.lock() {
                            *guard = Some(req_id.clone());
                        }
                    }
                }

                let prompt_msg = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "method": "session/prompt",
                    "params": {
                        "sessionId": session_id,
                        "prompt": prompt_parts
                    }
                });

                if let Err(e) = ws_to_agent_tx.send(prompt_msg.to_string()).await {
                    error!("⏰ Task '{}' failed: could not send session/prompt: {}", task.name, e);
                    if matches!(task.schedule, TaskSchedule::Once { .. }) {
                        if let Err(e) = delete_task_async(&tasks_dir, &task.id).await {
                            error!("⏰ Task '{}': also failed to delete task file: {}", task.name, e);
                        } else {
                            warn!("⏰ One-time task '{}' deleted after failure", task.name);
                        }
                    }
                    continue;
                }

                info!("⏰ Task prompt sent: '{}' (session={})", task.name, session_id);

                // Update task state
                match &task.schedule {
                    TaskSchedule::Once { .. } => {
                        // One-time tasks: delete the file after successful execution
                        if let Err(e) = delete_task_async(&tasks_dir, &task.id).await {
                            error!("⏰ Task '{}': failed to delete task file after execution: {}", task.name, e);
                        } else {
                            info!("⏰ One-time task '{}' completed and deleted", task.name);
                        }
                    }
                    TaskSchedule::Recurring { .. } => {
                        let mut updated_task = task.clone();
                        updated_task.last_run_at = Some(Utc::now().to_rfc3339());
                        updated_task.next_run_at = compute_next_run(&updated_task);
                        if let Err(e) = save_task_async(&tasks_dir, &updated_task).await {
                            error!("Failed to update task {} after execution: {}", task.id, e);
                        }
                    }
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_daily_task(time: &str, tz: &str) -> ScheduledTask {
        ScheduledTask {
            id: "test-daily".to_string(),
            name: "Test Daily".to_string(),
            prompt: "test".to_string(),
            schedule: TaskSchedule::Recurring {
                frequency: Frequency::Daily,
                time: time.to_string(),
                day_of_week: None,
                day_of_month: None,
                timezone: tz.to_string(),
            },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        }
    }

    #[test]
    fn test_compute_next_run_daily_returns_some() {
        let task = make_daily_task("04:00", "UTC");
        let next = compute_next_run(&task);
        assert!(next.is_some(), "Daily task should always have a next run");
    }

    #[test]
    fn test_compute_next_run_once_future() {
        let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
        let task = ScheduledTask {
            id: "test-once".to_string(),
            name: "Test Once".to_string(),
            prompt: "test".to_string(),
            schedule: TaskSchedule::Once { at: future.clone() },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        };
        let next = compute_next_run(&task);
        assert_eq!(next, Some(future));
    }

    #[test]
    fn test_compute_next_run_once_past() {
        let past = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let task = ScheduledTask {
            id: "test-once-past".to_string(),
            name: "Test Past".to_string(),
            prompt: "test".to_string(),
            schedule: TaskSchedule::Once { at: past },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        };
        let next = compute_next_run(&task);
        assert!(next.is_none(), "Past one-time task should return None");
    }

    #[test]
    fn test_save_and_load_task() {
        let dir = tempfile::tempdir().unwrap();
        let task = make_daily_task("09:00", "America/New_York");
        save_task(dir.path(), &task).unwrap();

        let tasks = load_tasks(dir.path());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, task.id);
        assert_eq!(tasks[0].name, "Test Daily");
    }

    #[test]
    fn test_delete_task() {
        let dir = tempfile::tempdir().unwrap();
        let task = make_daily_task("09:00", "UTC");
        save_task(dir.path(), &task).unwrap();
        assert_eq!(load_tasks(dir.path()).len(), 1);

        delete_task(dir.path(), &task.id).unwrap();
        assert_eq!(load_tasks(dir.path()).len(), 0);
    }

    #[test]
    fn test_ensure_default_tasks_creates_compact() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_tasks(dir.path()).len(), 0);

        ensure_default_tasks(dir.path());
        let tasks = load_tasks(dir.path());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, DEFAULT_COMPACT_MEMORY_NAME);
        assert!(tasks[0].enabled);
        assert!(tasks[0].next_run_at.is_some());
    }

    #[test]
    fn test_ensure_default_tasks_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        ensure_default_tasks(dir.path());
        ensure_default_tasks(dir.path());
        assert_eq!(load_tasks(dir.path()).len(), 1);
    }

    #[test]
    fn test_compute_next_run_weekly() {
        let task = ScheduledTask {
            id: "test-weekly".to_string(),
            name: "Weekly".to_string(),
            prompt: "test".to_string(),
            schedule: TaskSchedule::Recurring {
                frequency: Frequency::Weekly,
                time: "10:00".to_string(),
                day_of_week: Some(1), // Monday
                day_of_month: None,
                timezone: "UTC".to_string(),
            },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        };
        let next = compute_next_run(&task);
        assert!(next.is_some());
        // Should be within the next 7 days
        let next_dt = chrono::DateTime::parse_from_rfc3339(next.as_ref().unwrap()).unwrap();
        let diff = next_dt.signed_duration_since(Utc::now());
        assert!(diff.num_days() <= 7);
        assert!(diff.num_seconds() > 0);
    }

    #[test]
    fn test_compute_next_run_monthly() {
        let task = ScheduledTask {
            id: "test-monthly".to_string(),
            name: "Monthly".to_string(),
            prompt: "test".to_string(),
            schedule: TaskSchedule::Recurring {
                frequency: Frequency::Monthly,
                time: "08:00".to_string(),
                day_of_week: None,
                day_of_month: Some(15),
                timezone: "UTC".to_string(),
            },
            enabled: true,
            created_at: Utc::now().to_rfc3339(),
            last_run_at: None,
            next_run_at: None,
        };
        let next = compute_next_run(&task);
        assert!(next.is_some());
        let next_dt = chrono::DateTime::parse_from_rfc3339(next.as_ref().unwrap()).unwrap();
        assert!(next_dt > Utc::now());
    }

    #[test]
    fn test_task_json_roundtrip() {
        let task = ScheduledTask {
            id: "abc-123".to_string(),
            name: "My Task".to_string(),
            prompt: "Do something".to_string(),
            schedule: TaskSchedule::Recurring {
                frequency: Frequency::Weekly,
                time: "14:30".to_string(),
                day_of_week: Some(3),
                day_of_month: None,
                timezone: "Europe/Istanbul".to_string(),
            },
            enabled: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_run_at: None,
            next_run_at: Some("2026-01-08T11:30:00Z".to_string()),
        };

        let json = serde_json::to_string(&task).unwrap();
        let parsed: ScheduledTask = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, task.id);
        assert_eq!(parsed.name, task.name);
        if let TaskSchedule::Recurring { frequency, .. } = &parsed.schedule {
            assert_eq!(*frequency, Frequency::Weekly);
        } else {
            panic!("Expected Recurring schedule");
        }
    }
}
