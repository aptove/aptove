use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use agent_core::config::AgentConfig;
use agent_core::scheduler::SchedulerStore;
use agent_core::session::SessionManager;
use agent_core::transport::TransportSender;
use agent_core::trigger::TriggerStore;
use agent_core::AgentRuntime;
use agent_mcp_bridge::McpBridge;

pub struct AgentState {
    pub config: AgentConfig,
    pub runtime: RwLock<AgentRuntime>,
    pub session_manager: SessionManager,
    /// Maps session_id → workspace_uuid for persistence.
    pub session_workspaces: RwLock<HashMap<String, String>>,
    pub mcp_bridge: Option<Arc<tokio::sync::Mutex<McpBridge>>>,
    pub sender: TransportSender,
    /// Scheduler store for jobs/* ACP methods (None if not configured).
    pub scheduler_store: Option<Arc<dyn SchedulerStore>>,
    /// Trigger store for triggers/* ACP methods (None if not configured).
    pub trigger_store: Option<Arc<dyn TriggerStore>>,
}
