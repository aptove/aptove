use anyhow::Result;
use agent_core::config::AgentConfig;

use crate::commands::WorkspaceAction;
use crate::runtime::build_runtime;

pub async fn run_workspace_command(action: WorkspaceAction, config: AgentConfig) -> Result<()> {
    let runtime = build_runtime(config).await?;
    let wm = runtime.workspace_manager();

    match action {
        WorkspaceAction::List => {
            let workspaces = wm.list().await?;
            if workspaces.is_empty() {
                println!("No workspaces found.");
            } else {
                println!(
                    "{:<38} {:<20} {:<24} {}",
                    "UUID", "NAME", "LAST ACCESSED", "PROVIDER"
                );
                println!("{}", "-".repeat(100));
                for ws in workspaces {
                    println!(
                        "{:<38} {:<20} {:<24} {}",
                        ws.uuid,
                        ws.name.as_deref().unwrap_or("-"),
                        ws.last_accessed.format("%Y-%m-%d %H:%M:%S"),
                        ws.provider.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        WorkspaceAction::Create { name } => {
            let ws = wm.create(name.as_deref(), None).await?;
            println!("✅ Created workspace: {}", ws.uuid);
            if let Some(ref name) = ws.name {
                println!("   Name: {}", name);
            }
        }
        WorkspaceAction::Delete { uuid } => {
            wm.delete(&uuid).await?;
            println!("🗑  Deleted workspace: {}", uuid);
        }
        WorkspaceAction::Show { uuid } => {
            let ws = wm.load(&uuid).await?;
            println!("Workspace: {}", ws.uuid);
            println!(
                "  Name:          {}",
                ws.name.as_deref().unwrap_or("-")
            );
            println!(
                "  Provider:      {}",
                ws.provider.as_deref().unwrap_or("-")
            );
            println!(
                "  Created:       {}",
                ws.created_at.format("%Y-%m-%d %H:%M:%S")
            );
            println!(
                "  Last Accessed: {}",
                ws.last_accessed.format("%Y-%m-%d %H:%M:%S")
            );
        }
        WorkspaceAction::Gc { max_age } => {
            let removed = wm.gc(max_age).await?;
            println!(
                "🧹 Removed {} stale workspace(s) (older than {} days)",
                removed, max_age
            );
        }
    }

    Ok(())
}
