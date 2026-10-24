//! rigagent: a terminal AI agent built on Rig and Ratatui.
//!
//! Configuration comes from the command line and the environment (see
//! [`config`]); the interface runs in [`tui`] and the model in [`agent`].

mod agent;
mod config;
mod mcp;
mod providers;
mod tools;
mod tui;

use anyhow::Result;
use clap::Parser;

use config::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    config::load_dotenv()?;
    init_tracing();

    let config = Config::resolve(Cli::parse())?;
    tracing::info!(provider = config.provider.label(), model = %config.model, "starting");

    // `tui::run` restores the terminal before returning, so an error here can
    // be printed normally.
    if let Err(err) = tui::run(config).await {
        eprintln!("rigagent: {err:#}");
        std::process::exit(1);
    }
    Ok(())
}

/// Send logs to the file named by `RIGAGENT_LOG_FILE`, filtered by `RUST_LOG`.
///
/// Logging is off by default: the TUI owns the terminal, so anything written to
/// stdout or stderr while it runs would corrupt the display.
fn init_tracing() {
    let Some(path) = std::env::var_os("RIGAGENT_LOG_FILE") else {
        return;
    };
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("rigagent=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}
