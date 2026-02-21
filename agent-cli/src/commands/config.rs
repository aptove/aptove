use anyhow::Result;
use agent_core::config::{self, AgentConfig};

use crate::commands::ConfigAction;

pub fn run_config_command(action: ConfigAction, config: AgentConfig) -> Result<()> {
    match action {
        ConfigAction::Show => {
            let toml_str = toml::to_string_pretty(&config)?;
            println!("{}", toml_str);
        }
        ConfigAction::Init => {
            let path = AgentConfig::default_path()?;
            if path.exists() {
                eprintln!("Config already exists at: {}", path.display());
                eprintln!("Edit it directly or delete it first.");
            } else {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, config::sample_config())?;
                eprintln!("✅ Config written to: {}", path.display());
                eprintln!("   Edit it to add your API key and configure MCP servers.");
            }
        }
    }
    Ok(())
}
