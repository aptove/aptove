pub mod chat;
pub mod config;
pub mod jobs;
pub mod triggers;
pub mod workspace;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "aptove", version = "0.1.0", about = "Aptove — ACP AI Coding Agent")]
pub struct Cli {
    /// Path to config file
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// Target workspace UUID (default: auto-detect via device binding)
    #[arg(long, global = true)]
    pub workspace: Option<String>,

    /// Start in ACP stdio mode (alias for the `stdio` subcommand; for use with bridge)
    #[arg(long, global = true, hide = true)]
    pub stdio: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Run the ACP agent and WebSocket bridge in a single process (default)
    Run {
        /// Port to listen on (overrides config.toml [serve] port)
        #[arg(long)]
        port: Option<u16>,
        /// Enable TLS (overrides config.toml [serve] tls)
        #[arg(long)]
        tls: Option<bool>,
        /// Bind address (overrides config.toml [serve] bind_addr)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Show connection QR code (detects whether aptove is already running)
    ShowQr,
    /// Start in ACP stdio mode (for use with an external bridge)
    Stdio,
    /// Interactive REPL chat mode
    Chat,
    /// Workspace management
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Scheduled job management
    Jobs {
        #[command(subcommand)]
        action: JobsAction,
    },
    /// Webhook trigger management
    Triggers {
        #[command(subcommand)]
        action: TriggersAction,
    },
}

#[derive(Subcommand)]
pub enum JobsAction {
    /// List all scheduled jobs in a workspace
    List {
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Create a new scheduled job
    Create {
        /// Job display name
        #[arg(long)]
        name: String,
        /// Prompt to send to the LLM on each run
        #[arg(long)]
        prompt: String,
        /// Cron expression (5-field, e.g. "0 9 * * *" for 9 AM daily)
        #[arg(long)]
        schedule: String,
        /// IANA timezone (e.g. "America/New_York"). Defaults to UTC.
        #[arg(long)]
        timezone: Option<String>,
        /// Run only once then disable
        #[arg(long)]
        one_shot: bool,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show a job's details and latest output
    Show {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Manually trigger a job immediately
    Run {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Enable a job
    Enable {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Disable a job
    Disable {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Delete a job and its run history
    Delete {
        /// Job UUID
        job_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum TriggersAction {
    /// List all webhook triggers in a workspace
    List {
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Create a new webhook trigger
    Create {
        /// Trigger display name
        #[arg(long)]
        name: String,
        /// Prompt template (use {{payload}}, {{content_type}}, {{timestamp}}, {{trigger_name}}, {{headers}})
        #[arg(long)]
        prompt: String,
        /// Rate limit — max events per minute (0 = unlimited)
        #[arg(long, default_value = "60")]
        rate_limit: u32,
        /// Optional HMAC-SHA256 secret for signature verification
        #[arg(long)]
        hmac_secret: Option<String>,
        /// Accepted content types (comma-separated, empty = any)
        #[arg(long)]
        content_types: Option<String>,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Show a trigger's details and latest run
    Show {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Send a test event to a trigger (executes the prompt template)
    Test {
        /// Trigger UUID
        trigger_id: String,
        /// Test payload body
        #[arg(long, default_value = "{}")]
        payload: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Enable a trigger
    Enable {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Disable a trigger
    Disable {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Delete a trigger and its run history
    Delete {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
    /// Regenerate the webhook token (invalidates the old URL)
    RegenerateToken {
        /// Trigger UUID
        trigger_id: String,
        /// Workspace UUID (default: uses default workspace)
        #[arg(long)]
        workspace: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum WorkspaceAction {
    /// List all workspaces
    List,
    /// Create a new workspace
    Create {
        /// Optional workspace name
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a workspace
    Delete {
        /// Workspace UUID
        uuid: String,
    },
    /// Show workspace details
    Show {
        /// Workspace UUID
        uuid: String,
    },
    /// Garbage-collect stale workspaces
    Gc {
        /// Maximum age in days (default: 90)
        #[arg(long, default_value = "90")]
        max_age: u64,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Show the resolved configuration
    Show,
    /// Generate a sample config file
    Init,
}
