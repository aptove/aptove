use std::sync::{Arc, atomic::AtomicU8};

use anyhow::Result;
use tokio::sync::mpsc;
use tracing_subscriber::prelude::*;

use aptove_core::common_config::CommonConfig;
use aptove_core::transport;
use aptove_core::file_log_layer::{self, FileLogLayer};
use aptove_core::tui::{
    app::App,
    events::AppEvent,
    log_layer::{TuiLogLayer, level_name_to_u8},
};

mod plugins;
mod updater;

#[tokio::main]
async fn main() -> Result<()> {
    // Install ring as the process-level rustls crypto provider before any TLS work.
    // Required when multiple crates in the process use rustls 0.23+ (e.g. tokio-rustls
    // and self_update/ureq both use rustls; without this call rustls panics at runtime).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Register OSS transports: Local and Tailscale.
    transport::init(plugins::oss_transports());

    run_tui().await
}

async fn run_tui() -> Result<()> {
    let mut config = CommonConfig::load()?;
    config.ensure_agent_id();
    config.ensure_auth_token();
    config.save()?;

    let (event_tx, event_rx) = mpsc::channel::<AppEvent>(512);
    let log_level_arc = Arc::new(AtomicU8::new(level_name_to_u8(&config.log_level)));

    let log_layer = TuiLogLayer::new(event_tx.clone(), Arc::clone(&log_level_arc));
    let logs_dir = CommonConfig::config_dir().join("logs");
    let file_log = FileLogLayer::new(logs_dir, config.log_file_max_mb, config.log_file_count);
    file_log_layer::set_log_message_content(config.log_message_content);
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("trace"))
        .with(log_layer)
        .with(file_log)
        .init();

    let tick_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            interval.tick().await;
            if tick_tx.send(AppEvent::Tick).await.is_err() { break; }
        }
    });

    // Background version check — fires once at startup, non-blocking.
    let update_check_tx = event_tx.clone();
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(updater::check_for_update).await;
        let version = result.ok().flatten();
        let _ = update_check_tx.send(AppEvent::UpdateCheckResult(version)).await;
    });

    // Update trigger channel: App sends () here when F5 is pressed;
    // the watcher task below does the actual download+replace.
    let (update_trigger_tx, mut update_trigger_rx) = tokio::sync::mpsc::channel::<()>(1);
    let update_download_tx = event_tx.clone();
    tokio::spawn(async move {
        while update_trigger_rx.recv().await.is_some() {
            let _ = update_download_tx
                .send(AppEvent::UpdateProgress("Downloading update...".to_string()))
                .await;
            let result = tokio::task::spawn_blocking(updater::perform_update)
                .await
                .unwrap_or_else(|_| Err("Update task panicked".to_string()));
            let _ = update_download_tx.send(AppEvent::UpdateComplete(result)).await;
        }
    });

    let app = App::new(config, event_tx, log_level_arc, Some(update_trigger_tx));
    app.run(event_rx).await
}
