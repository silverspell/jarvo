//! rigagent: a terminal AI agent built on Rig and Ratatui.
//!
//! Configuration comes from the command line and the environment (see
//! [`config`]); the interface runs in [`tui`] and the model in [`agent`].

mod agent;
mod command;
mod config;
mod docs;
mod mcp;
mod office;
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

    let cli = Cli::parse();

    // The server is its own process: it needs no provider, model or API key.
    if let Some(dir) = config::non_empty_path(cli.serve_office.clone()) {
        let bind = config::office_bind_or_default(cli.office_bind.clone());
        if let Err(err) = office::server::serve(&dir, &bind).await {
            eprintln!("rigagent: {err:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let config = Config::resolve(cli)?;
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

    let user = std::env::var("RUST_LOG").ok();
    let filter = tracing_subscriber::EnvFilter::try_new(log_filter_spec(user.as_deref()))
        .unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(format!("rigagent=info,{TRANSPORT_LOG_CAP}"))
        });
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file)
        .with_ansi(false)
        .try_init();
}

/// Transport crates log the WebSocket handshake URL at trace, token included.
/// The cap is appended after the user's directives, so it wins.
const TRANSPORT_LOG_CAP: &str = "tungstenite=info,tokio_tungstenite=info,hyper=info,axum=info";

/// The filter spec: the user's wishes first, the transport cap last so it
/// cannot be raised past `info` by `RUST_LOG=trace` or a per-crate directive.
fn log_filter_spec(user: Option<&str>) -> String {
    let user = user.map(str::trim).filter(|user| !user.is_empty());
    let base = user.unwrap_or("rigagent=info");
    format!("{base},{TRANSPORT_LOG_CAP}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transport_cap_follows_the_user_filter_so_it_wins() {
        let spec = log_filter_spec(Some("trace"));
        assert!(spec.starts_with("trace,"), "{spec}");
        assert!(
            spec.ends_with(TRANSPORT_LOG_CAP),
            "the cap must be the last directive: {spec}"
        );

        let spec = log_filter_spec(Some("tungstenite=trace"));
        assert!(
            spec.ends_with(TRANSPORT_LOG_CAP),
            "a per-crate override cannot beat the cap: {spec}"
        );
    }

    #[test]
    fn a_missing_or_blank_user_filter_still_gets_the_cap() {
        for user in [None, Some(""), Some("   ")] {
            let spec = log_filter_spec(user);
            assert!(spec.starts_with("rigagent=info,"), "{spec}");
            assert!(spec.ends_with(TRANSPORT_LOG_CAP), "{spec}");
        }
    }

    #[test]
    fn the_capped_spec_is_a_valid_filter() {
        for user in [None, Some("trace"), Some("rigagent=debug,hyper=trace")] {
            assert!(
                tracing_subscriber::EnvFilter::try_new(log_filter_spec(user)).is_ok(),
                "spec for {user:?} parses"
            );
        }
    }
}
