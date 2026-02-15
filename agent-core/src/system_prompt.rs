//! System Prompt Manager
//!
//! Loads and manages system prompts: global defaults, per-mode prompts,
//! project-level overrides from `.Aptove/system_prompt.md`, and
//! template variable substitution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::{debug, info};

use crate::plugin::{Message, MessageContent, Role};

// ---------------------------------------------------------------------------
// System Prompt Manager
// ---------------------------------------------------------------------------

/// Manages system prompt loading, overrides, and template substitution.
pub struct SystemPromptManager {
    /// Default system prompt text.
    default_prompt: String,
    /// Per-mode prompts (mode name → prompt text).
    mode_prompts: HashMap<String, String>,
    /// Working directory for project-level overrides.
    working_dir: PathBuf,
    /// Cached project-level override (loaded lazily).
    project_override: Option<String>,
}

impl SystemPromptManager {
    /// Create a new system prompt manager.
    pub fn new(default_prompt: &str, working_dir: &Path) -> Self {
        Self {
            default_prompt: default_prompt.to_string(),
            mode_prompts: HashMap::new(),
            working_dir: working_dir.to_path_buf(),
            project_override: None,
        }
    }

    /// Set a per-mode system prompt.
    pub fn set_mode_prompt(&mut self, mode: &str, prompt: &str) {
        self.mode_prompts.insert(mode.to_string(), prompt.to_string());
    }

    /// Load the project-level override if it exists.
    /// Looks for `.Aptove/system_prompt.md` in the working directory.
    pub fn load_project_override(&mut self) -> Result<bool> {
        let path = self.working_dir.join(".Aptove").join("system_prompt.md");
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            info!(path = %path.display(), "loaded project system prompt override");
            self.project_override = Some(content);
            Ok(true)
        } else {
            debug!("no project system prompt at {}", path.display());
            self.project_override = None;
            Ok(false)
        }
    }

    /// Get the resolved system prompt for a given mode.
    ///
    /// Priority: project override > mode-specific > default.
    pub fn resolve(&self, mode: Option<&str>) -> &str {
        // 1. Project-level override takes highest priority
        if let Some(ref override_prompt) = self.project_override {
            return override_prompt;
        }

        // 2. Mode-specific prompt
        if let Some(mode) = mode {
            if let Some(prompt) = self.mode_prompts.get(mode) {
                return prompt;
            }
        }

        // 3. Default
        &self.default_prompt
    }

    /// Resolve the system prompt and apply template variable substitution.
    pub fn resolve_with_variables(
        &self,
        mode: Option<&str>,
        variables: &HashMap<String, String>,
    ) -> String {
        let mut prompt = self.resolve(mode).to_string();

        for (key, value) in variables {
            let placeholder = format!("{{{{{}}}}}", key); // {{key}}
            prompt = prompt.replace(&placeholder, value);
        }

        prompt
    }

    /// Build a system Message from the resolved prompt.
    pub fn build_message(
        &self,
        mode: Option<&str>,
        variables: &HashMap<String, String>,
    ) -> Message {
        let text = self.resolve_with_variables(mode, variables);
        Message {
            role: Role::System,
            content: MessageContent::Text(text),
        }
    }

    /// Build default template variables from current state.
    pub fn default_variables(
        provider: &str,
        model: &str,
        tools: &[String],
    ) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("provider".to_string(), provider.to_string());
        vars.insert("model".to_string(), model.to_string());
        vars.insert("tools".to_string(), tools.join(", "));
        vars
    }
}

/// Default system prompt for the agent.
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are Aptove, an AI coding assistant. You help developers write, debug, and understand code.

Current provider: {{provider}}
Current model: {{model}}
Available tools: {{tools}}

Guidelines:
- Be concise and direct
- Show code examples when helpful
- Explain your reasoning for complex changes
- Ask clarifying questions when the request is ambiguous
- Respect the project's coding conventions and style"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn resolve_default() {
        let mgr = SystemPromptManager::new("default prompt", Path::new("/tmp"));
        assert_eq!(mgr.resolve(None), "default prompt");
    }

    #[test]
    fn resolve_mode_specific() {
        let mut mgr = SystemPromptManager::new("default", Path::new("/tmp"));
        mgr.set_mode_prompt("planning", "You are in planning mode.");

        assert_eq!(mgr.resolve(Some("planning")), "You are in planning mode.");
        assert_eq!(mgr.resolve(Some("coding")), "default");
        assert_eq!(mgr.resolve(None), "default");
    }

    #[test]
    fn template_substitution() {
        let mgr = SystemPromptManager::new(
            "Using {{provider}} with {{model}}. Tools: {{tools}}",
            Path::new("/tmp"),
        );

        let vars = SystemPromptManager::default_variables(
            "claude",
            "claude-sonnet-4-20250514",
            &["bash".to_string(), "filesystem".to_string()],
        );

        let result = mgr.resolve_with_variables(None, &vars);
        assert!(result.contains("claude"));
        assert!(result.contains("claude-sonnet-4-20250514"));
        assert!(result.contains("bash, filesystem"));
    }

    #[test]
    fn project_override_not_found() {
        let mut mgr = SystemPromptManager::new("default", Path::new("/tmp/nonexistent"));
        let found = mgr.load_project_override().unwrap();
        assert!(!found);
        assert_eq!(mgr.resolve(None), "default");
    }
}
