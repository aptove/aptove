//! Git Integration Plugin
//!
//! Provides git-awareness to the agent:
//! - On session start: detects git repo; warns if the repo has uncommitted changes.
//! - Before file-write tool calls: warns if the target file has uncommitted changes.
//! - After file-write tool calls: stages the file with `git add`.
//! - On each LLM turn boundary (`on_output_chunk(is_final=true)`): commits all files
//!   staged during that turn as a single, descriptively-named commit.
//! - On shutdown: flushes any remaining staged files.
//!
//! ## Config (TOML section `[plugins.git]`)
//! ```toml
//! [plugins.git]
//! auto_commit = true           # commit after each agent turn
//! commit_message_prefix = "agent: "
//! warn_dirty = true            # warn if repo has uncommitted changes
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use agent_core::plugin::{OutputChunk, Plugin};
use agent_core::types::{ToolCallRequest, ToolCallResult};
use agent_core::session::Session;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Tool name substrings that indicate a file-write operation.
const WRITE_TOOL_NAMES: &[&str] = &[
    "write_file",
    "create_file",
    "edit_file",
    "str_replace",
    "overwrite_file",
    "apply_patch",
    "write",
];

/// Argument keys that may hold the target file path.
const PATH_ARG_KEYS: &[&str] = &["path", "file_path", "filename", "file", "target_file"];

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// User-facing configuration for the git plugin.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitConfig {
    /// Whether to automatically commit file writes at the end of each agent turn.
    pub auto_commit: bool,
    /// Prefix used in auto-generated commit messages.
    pub commit_message_prefix: String,
    /// Whether to warn when the repo (or target file) has uncommitted changes.
    pub warn_dirty: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_commit: true,
            commit_message_prefix: "agent: ".to_string(),
            warn_dirty: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

/// Runtime state shared across hook calls (needs interior mutability because
/// all Plugin hooks except `on_init` take `&self`).
struct GitState {
    /// True once `on_session_start` confirms we are inside a git repository.
    in_git_repo: bool,
    /// Absolute path to the repository root (`git rev-parse --show-toplevel`).
    repo_root: Option<PathBuf>,
    /// Files staged during the current agent turn, waiting to be grouped into
    /// a single commit when the turn ends.
    pending_files: Vec<String>,
}

impl Default for GitState {
    fn default() -> Self {
        Self {
            in_git_repo: false,
            repo_root: None,
            pending_files: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin struct
// ---------------------------------------------------------------------------

/// Git integration plugin.
pub struct GitPlugin {
    /// Configuration — set once in `on_init` (which has `&mut self`), then
    /// read-only for all subsequent `&self` hooks.
    config: GitConfig,
    /// Mutable runtime state behind a Mutex for `&self` hooks.
    state: Arc<Mutex<GitState>>,
}

impl GitPlugin {
    /// Create a new git plugin with default configuration.
    pub fn new() -> Self {
        Self {
            config: GitConfig::default(),
            state: Arc::new(Mutex::new(GitState::default())),
        }
    }

    /// Generate a commit message summarising the set of written files.
    ///
    /// Examples:
    /// - 1 file  → `"agent: update main.rs"`
    /// - 2 files → `"agent: update 2 files (lib.rs, main.rs)"`
    /// - 4 files → `"agent: update 4 files (a.rs, b.rs, c.rs and 1 more)"`
    fn generate_commit_message(&self, files: &[String]) -> String {
        let prefix = &self.config.commit_message_prefix;
        match files.len() {
            0 => format!("{}update files", prefix),
            1 => format!("{}update {}", prefix, short_name(&files[0])),
            n => {
                let names: Vec<&str> = files.iter().map(|f| short_name(f)).collect();
                let summary = if n <= 3 {
                    names.join(", ")
                } else {
                    format!("{} and {} more", names[..3].join(", "), n - 3)
                };
                format!("{}update {} files ({})", prefix, n, summary)
            }
        }
    }

    /// Commit all pending staged files as a single git commit, then clear the
    /// pending list. No-ops if there are no pending files or auto_commit is off.
    async fn flush_pending_commit(&self) {
        let (files, repo_root) = {
            let mut state = self.state.lock().await;
            if !state.in_git_repo || !self.config.auto_commit || state.pending_files.is_empty() {
                return;
            }
            let files = std::mem::take(&mut state.pending_files);
            let root = match state.repo_root.clone() {
                Some(r) => r,
                None => return,
            };
            (files, root)
        };

        // Only commit if git actually has staged changes (files may be identical
        // to what was already committed).
        match has_staged_changes(&repo_root).await {
            Ok(true) => {
                let message = self.generate_commit_message(&files);
                match git_commit(&repo_root, &message).await {
                    Ok(_) => info!("[git] Committed {} file(s): \"{}\"", files.len(), message),
                    Err(e) => warn!("[git] Commit failed: {}", e),
                }
            }
            Ok(false) => debug!("[git] Pending files staged but no changes to commit"),
            Err(e) => warn!("[git] Could not check staged changes: {}", e),
        }
    }
}

impl Default for GitPlugin {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Plugin trait implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Plugin for GitPlugin {
    fn name(&self) -> &str {
        "git"
    }

    // 2.9 Config loading
    async fn on_init(&mut self, config: &serde_json::Value) -> Result<()> {
        // Attempt full deserialization first; fall back to field-by-field.
        if let Ok(cfg) = serde_json::from_value::<GitConfig>(config.clone()) {
            self.config = cfg;
        } else {
            if let Some(v) = config.get("auto_commit").and_then(|v| v.as_bool()) {
                self.config.auto_commit = v;
            }
            if let Some(v) = config.get("commit_message_prefix").and_then(|v| v.as_str()) {
                self.config.commit_message_prefix = v.to_string();
            }
            if let Some(v) = config.get("warn_dirty").and_then(|v| v.as_bool()) {
                self.config.warn_dirty = v;
            }
        }
        debug!("[git] initialized: {:?}", self.config);
        Ok(())
    }

    /// Flush pending commits before the agent shuts down.
    async fn on_shutdown(&self) -> Result<()> {
        self.flush_pending_commit().await;
        Ok(())
    }

    // 2.2 / 2.3 / 2.4 Repo detection + dirty warning at session start
    async fn on_session_start(&self, session: &mut Session) -> Result<()> {
        match detect_git_repo().await {
            Some(root) => {
                info!("[git] Repository root: {}", root.display());
                {
                    let mut state = self.state.lock().await;
                    state.in_git_repo = true;
                    state.repo_root = Some(root.clone());
                }

                if self.config.warn_dirty {
                    match check_dirty(&root).await {
                        Ok(dirty) if !dirty.is_empty() => {
                            warn!("[git] {} uncommitted file(s) in repo", dirty.len());
                            session.add_user_message(&format!(
                                "System notice: The git repository has {} file(s) with \
                                 uncommitted changes. The agent will auto-commit its own \
                                 file writes.",
                                dirty.len()
                            ));
                        }
                        Ok(_) => debug!("[git] Repository is clean"),
                        Err(e) => warn!("[git] Could not check dirty state: {}", e),
                    }
                }
            }
            None => {
                debug!("[git] Not inside a git repository, plugin inactive");
            }
        }
        Ok(())
    }

    // 2.5 Warn before overwriting a file that has uncommitted changes
    async fn on_before_tool_call(&self, call: &mut ToolCallRequest) -> Result<()> {
        if !self.config.warn_dirty {
            return Ok(());
        }

        let (in_repo, repo_root) = {
            let state = self.state.lock().await;
            (state.in_git_repo, state.repo_root.clone())
        };

        if !in_repo {
            return Ok(());
        }
        let repo_root = match repo_root {
            Some(r) => r,
            None => return Ok(()),
        };

        if let Some(file_path) = extract_file_path(call) {
            match check_file_dirty(&repo_root, &file_path).await {
                Ok(true) => warn!(
                    "[git] '{}' has uncommitted changes that will be overwritten",
                    file_path
                ),
                Ok(false) => {}
                Err(e) => debug!("[git] Could not check dirty for '{}': {}", file_path, e),
            }
        }
        Ok(())
    }

    // 2.6 Stage written file and queue it for the next grouped commit
    async fn on_after_tool_call(
        &self,
        call: &ToolCallRequest,
        result: &mut ToolCallResult,
    ) -> Result<()> {
        // Skip failed tool calls — nothing was written.
        if result.is_error {
            return Ok(());
        }

        let (in_repo, repo_root) = {
            let state = self.state.lock().await;
            (state.in_git_repo, state.repo_root.clone())
        };

        if !in_repo {
            return Ok(());
        }
        let repo_root = match repo_root {
            Some(r) => r,
            None => return Ok(()),
        };

        if let Some(file_path) = extract_file_path(call) {
            match git_add(&repo_root, &file_path).await {
                Ok(_) => {
                    debug!("[git] Staged '{}'", file_path);
                    let mut state = self.state.lock().await;
                    // Deduplicate: if the same file was written twice this turn,
                    // only record it once.
                    if !state.pending_files.contains(&file_path) {
                        state.pending_files.push(file_path);
                    }
                }
                Err(e) => warn!("[git] Failed to stage '{}': {}", file_path, e),
            }
        }
        Ok(())
    }

    // 2.7 Turn-based commit grouping: flush staged files at end of each LLM turn
    async fn on_output_chunk(&self, chunk: &mut OutputChunk) -> Result<()> {
        if chunk.is_final {
            self.flush_pending_commit().await;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tool call helpers
// ---------------------------------------------------------------------------

/// Return the target file path if `call` looks like a file-write operation,
/// or `None` if it is some other kind of tool call.
fn extract_file_path(call: &ToolCallRequest) -> Option<String> {
    let name_lower = call.name.to_lowercase();
    let is_write = WRITE_TOOL_NAMES
        .iter()
        .any(|n| name_lower == *n || name_lower.contains(n));
    if !is_write {
        return None;
    }
    for key in PATH_ARG_KEYS {
        if let Some(val) = call.arguments.get(key).and_then(|v| v.as_str()) {
            return Some(val.to_string());
        }
    }
    None
}

/// Extract just the filename from a path string (last `/`-separated component).
fn short_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Git command helpers (public for integration testing)
// ---------------------------------------------------------------------------

/// Run a git sub-command in `cwd` and return stdout on success.
///
/// The raw stdout is returned without trimming — callers that interpret the
/// output as a single scalar value should call `.trim()` themselves.
/// Preserving leading whitespace is critical for line-oriented formats such as
/// `git status --porcelain`, where the leading character encodes index status.
async fn run_git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim())
    }
}

/// Detect the git repository root for the current working directory.
/// Returns `None` when the cwd is not inside any git repository.
pub async fn detect_git_repo() -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok()?;

    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Some(PathBuf::from(path))
    } else {
        None
    }
}

/// Return the list of files that have uncommitted changes in `repo_root`
/// (`git status --porcelain`).
pub async fn check_dirty(repo_root: &Path) -> Result<Vec<String>> {
    let output = run_git(&["status", "--porcelain"], repo_root).await?;
    let files = output
        .lines()
        .filter(|l| !l.trim().is_empty())
        // Each porcelain line is: `XY filename` — skip the 3-char prefix.
        .filter_map(|l| l.get(3..))
        .map(|f| f.trim().to_string())
        .collect();
    Ok(files)
}

/// Return `true` if `file_path` (relative or absolute) has uncommitted changes
/// according to git.
pub async fn check_file_dirty(repo_root: &Path, file_path: &str) -> Result<bool> {
    // Trimming the scalar output is safe here because we only check emptiness.
    let output = run_git(&["status", "--porcelain", "--", file_path], repo_root).await?;
    Ok(!output.trim().is_empty())
}

/// Return `true` if there are changes in the git index (staged but not committed).
pub async fn has_staged_changes(repo_root: &Path) -> Result<bool> {
    let output = run_git(&["diff", "--cached", "--name-only"], repo_root).await?;
    Ok(!output.trim().is_empty())
}

/// Stage `file_path` with `git add`.
pub async fn git_add(repo_root: &Path, file_path: &str) -> Result<()> {
    run_git(&["add", "--", file_path], repo_root).await?;
    Ok(())
}

/// Commit all staged changes with `message`.
pub async fn git_commit(repo_root: &Path, message: &str) -> Result<()> {
    run_git(&["commit", "-m", message], repo_root).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test repo helpers
    // -----------------------------------------------------------------------

    /// Initialise a throw-away git repo with a first commit so that HEAD exists.
    async fn init_test_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        cmd("git", &["init"], p).await;
        cmd("git", &["config", "user.email", "test@example.com"], p).await;
        cmd("git", &["config", "user.name", "Test Agent"], p).await;
        fs::write(p.join("README.md"), "initial\n").unwrap();
        cmd("git", &["add", "."], p).await;
        cmd("git", &["commit", "-m", "init"], p).await;
        dir
    }

    async fn cmd(program: &str, args: &[&str], cwd: &Path) {
        let status = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .status()
            .await
            .unwrap_or_else(|e| panic!("{} {:?} failed to start: {}", program, args, e));
        assert!(status.success(), "{} {:?} exited with {}", program, args, status);
    }

    // -----------------------------------------------------------------------
    // 2.2 Git repo detection
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_repo_detection_inside_git_repo() {
        let dir = init_test_repo().await;
        // rev-parse should succeed inside the repo.
        let out = Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "rev-parse should succeed inside a repo");
    }

    #[tokio::test]
    async fn test_repo_detection_outside_git_repo() {
        let dir = TempDir::new().unwrap(); // plain directory, no .git
        let out = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(!out.status.success(), "rev-parse should fail outside a repo");
    }

    // -----------------------------------------------------------------------
    // 2.3 Dirty state check
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_check_dirty_clean_repo() {
        let dir = init_test_repo().await;
        let dirty = check_dirty(dir.path()).await.unwrap();
        assert!(dirty.is_empty(), "newly initialised repo should be clean; got {:?}", dirty);
    }

    #[tokio::test]
    async fn test_check_dirty_untracked_file() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("untracked.txt"), "hello").unwrap();
        let dirty = check_dirty(dir.path()).await.unwrap();
        assert!(!dirty.is_empty(), "expected dirty after adding untracked file");
        assert!(dirty.iter().any(|f| f.contains("untracked.txt")));
    }

    #[tokio::test]
    async fn test_check_dirty_modified_tracked_file() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("README.md"), "modified\n").unwrap();
        let dirty = check_dirty(dir.path()).await.unwrap();
        assert!(!dirty.is_empty());
        assert!(dirty.iter().any(|f| f.contains("README.md")));
    }

    #[tokio::test]
    async fn test_check_file_dirty_clean() {
        let dir = init_test_repo().await;
        assert!(!check_file_dirty(dir.path(), "README.md").await.unwrap());
    }

    #[tokio::test]
    async fn test_check_file_dirty_modified() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("README.md"), "changed\n").unwrap();
        assert!(check_file_dirty(dir.path(), "README.md").await.unwrap());
    }

    // -----------------------------------------------------------------------
    // 2.6 Stage and commit (helpers)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_git_add_and_commit_clean_after() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("output.txt"), "agent wrote this").unwrap();
        git_add(dir.path(), "output.txt").await.unwrap();
        git_commit(dir.path(), "agent: update output.txt").await.unwrap();
        let dirty = check_dirty(dir.path()).await.unwrap();
        assert!(dirty.is_empty(), "repo should be clean after commit; got {:?}", dirty);
    }

    #[tokio::test]
    async fn test_has_staged_changes_false_before_add() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("new.txt"), "content").unwrap();
        // Not yet staged
        assert!(!has_staged_changes(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn test_has_staged_changes_true_after_add() {
        let dir = init_test_repo().await;
        fs::write(dir.path().join("new.txt"), "content").unwrap();
        git_add(dir.path(), "new.txt").await.unwrap();
        assert!(has_staged_changes(dir.path()).await.unwrap());
    }

    // -----------------------------------------------------------------------
    // 2.7 Turn-based commit grouping (grouped files → one commit)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_auto_commit_groups_multiple_files() {
        let dir = init_test_repo().await;
        let root = dir.path();

        // Simulate three file writes from one agent turn.
        let files = ["file_a.txt", "file_b.txt", "file_c.txt"];
        for f in &files {
            fs::write(root.join(f), format!("content of {}", f)).unwrap();
            git_add(root, f).await.unwrap();
        }

        let plugin = GitPlugin::new();
        let file_list: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        let message = plugin.generate_commit_message(&file_list);

        git_commit(root, &message).await.unwrap();

        // One commit should leave the repo clean.
        assert!(check_dirty(root).await.unwrap().is_empty());
        // The message should mention all three files as a group.
        assert!(message.starts_with("agent: "), "got: {}", message);
        assert!(message.contains("3 files"), "got: {}", message);
    }

    // -----------------------------------------------------------------------
    // 2.8 Commit message generation
    // -----------------------------------------------------------------------

    #[test]
    fn test_commit_message_single_file() {
        let plugin = GitPlugin::new();
        assert_eq!(
            plugin.generate_commit_message(&["src/main.rs".to_string()]),
            "agent: update main.rs"
        );
    }

    #[test]
    fn test_commit_message_two_files() {
        let plugin = GitPlugin::new();
        assert_eq!(
            plugin.generate_commit_message(&[
                "src/lib.rs".to_string(),
                "src/main.rs".to_string()
            ]),
            "agent: update 2 files (lib.rs, main.rs)"
        );
    }

    #[test]
    fn test_commit_message_three_files_no_ellipsis() {
        let plugin = GitPlugin::new();
        let msg = plugin.generate_commit_message(&[
            "a.rs".to_string(),
            "b.rs".to_string(),
            "c.rs".to_string(),
        ]);
        assert_eq!(msg, "agent: update 3 files (a.rs, b.rs, c.rs)");
    }

    #[test]
    fn test_commit_message_four_files_with_overflow() {
        let plugin = GitPlugin::new();
        let msg = plugin.generate_commit_message(&[
            "a.rs".to_string(),
            "b.rs".to_string(),
            "c.rs".to_string(),
            "d.rs".to_string(),
        ]);
        assert_eq!(msg, "agent: update 4 files (a.rs, b.rs, c.rs and 1 more)");
    }

    #[test]
    fn test_commit_message_custom_prefix() {
        let plugin = GitPlugin {
            config: GitConfig {
                auto_commit: true,
                commit_message_prefix: "bot: ".to_string(),
                warn_dirty: true,
            },
            state: Arc::new(Mutex::new(GitState::default())),
        };
        assert_eq!(
            plugin.generate_commit_message(&["foo.txt".to_string()]),
            "bot: update foo.txt"
        );
    }

    // -----------------------------------------------------------------------
    // 2.9 Config loading via on_init
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_config_full_deserialization() {
        let mut plugin = GitPlugin::new();
        plugin
            .on_init(&serde_json::json!({
                "auto_commit": false,
                "commit_message_prefix": "ci: ",
                "warn_dirty": false
            }))
            .await
            .unwrap();
        assert!(!plugin.config.auto_commit);
        assert_eq!(plugin.config.commit_message_prefix, "ci: ");
        assert!(!plugin.config.warn_dirty);
    }

    #[tokio::test]
    async fn test_config_null_keeps_defaults() {
        let mut plugin = GitPlugin::new();
        plugin.on_init(&serde_json::Value::Null).await.unwrap();
        assert!(plugin.config.auto_commit);
        assert_eq!(plugin.config.commit_message_prefix, "agent: ");
        assert!(plugin.config.warn_dirty);
    }

    #[tokio::test]
    async fn test_config_partial_overrides() {
        let mut plugin = GitPlugin::new();
        // Only override auto_commit; others should keep defaults.
        plugin
            .on_init(&serde_json::json!({ "auto_commit": false }))
            .await
            .unwrap();
        assert!(!plugin.config.auto_commit);
        assert_eq!(plugin.config.commit_message_prefix, "agent: "); // default
        assert!(plugin.config.warn_dirty); // default
    }

    // -----------------------------------------------------------------------
    // Tool-call file-path extraction
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_file_path_write_file_with_path_arg() {
        let call = ToolCallRequest {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/foo.txt", "content": "hi"}),
        };
        assert_eq!(extract_file_path(&call), Some("/tmp/foo.txt".to_string()));
    }

    #[test]
    fn test_extract_file_path_edit_file_with_file_path_arg() {
        let call = ToolCallRequest {
            id: "2".into(),
            name: "edit_file".into(),
            arguments: serde_json::json!({"file_path": "src/main.rs"}),
        };
        assert_eq!(extract_file_path(&call), Some("src/main.rs".to_string()));
    }

    #[test]
    fn test_extract_file_path_non_write_tool_returns_none() {
        let call = ToolCallRequest {
            id: "3".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "ls"}),
        };
        assert_eq!(extract_file_path(&call), None);
    }

    #[test]
    fn test_extract_file_path_write_tool_without_path_arg_returns_none() {
        let call = ToolCallRequest {
            id: "4".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"content": "hello"}),
        };
        assert_eq!(extract_file_path(&call), None);
    }

    #[test]
    fn test_short_name_strips_directory() {
        assert_eq!(short_name("src/plugin/git.rs"), "git.rs");
        assert_eq!(short_name("file.txt"), "file.txt");
        assert_eq!(short_name("/absolute/path/to/file.rs"), "file.rs");
    }

    // -----------------------------------------------------------------------
    // on_before_tool_call: no error returned (warnings are non-fatal)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_before_tool_call_noop_outside_repo() {
        // Plugin with in_git_repo=false (default) — should be a clean no-op.
        let plugin = GitPlugin::new();
        let mut call = ToolCallRequest {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
        };
        // Must not return an error.
        plugin.on_before_tool_call(&mut call).await.unwrap();
    }

    // -----------------------------------------------------------------------
    // on_after_tool_call: skips failed tool results
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_after_tool_call_skips_on_error_result() {
        let plugin = GitPlugin::new();
        let call = ToolCallRequest {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
        };
        let mut result = ToolCallResult {
            tool_call_id: "1".into(),
            content: "write failed".into(),
            is_error: true,
        };
        plugin.on_after_tool_call(&call, &mut result).await.unwrap();
        // No files should have been queued.
        let state = plugin.state.lock().await;
        assert!(state.pending_files.is_empty());
    }

    // -----------------------------------------------------------------------
    // on_after_tool_call: deduplicates same file written twice in one turn
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_pending_files_deduplicated() {
        let dir = init_test_repo().await;
        let root = dir.path().to_path_buf();

        // Pre-set state so the plugin believes it's in a repo.
        let plugin = GitPlugin::new();
        {
            let mut state = plugin.state.lock().await;
            state.in_git_repo = true;
            state.repo_root = Some(root.clone());
        }

        // Write and stage the same file twice.
        fs::write(root.join("dup.txt"), "v1").unwrap();

        let call = ToolCallRequest {
            id: "1".into(),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "dup.txt"}),
        };
        let mut ok = ToolCallResult {
            tool_call_id: "1".into(),
            content: "ok".into(),
            is_error: false,
        };

        plugin.on_after_tool_call(&call, &mut ok).await.unwrap();
        plugin.on_after_tool_call(&call, &mut ok).await.unwrap();

        let state = plugin.state.lock().await;
        assert_eq!(state.pending_files.len(), 1, "same file should not be queued twice");
    }
}
