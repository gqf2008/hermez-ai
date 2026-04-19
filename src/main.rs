//! Hermes Agent CLI — main entry point.
//!
//! Replaces the Python `hermes` command (hermes_cli.main:main).
//! Supports subcommands: chat, setup, tools, skills, gateway, doctor, etc.
//!
//! Command schemas and dispatch logic have been extracted to `src/commands/`.

mod commands;

use clap::Parser;
use hermes_cli::app::HermesApp;
use tracing_subscriber::EnvFilter;

use commands::{Cli, dispatch::dispatch_command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    if cli.verbose {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("debug"))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .init();
    }

    // Set Hermes home if provided
    if let Some(home) = cli.hermes_home {
        hermes_core::hermes_home::set_hermes_home(&home)
            .ok();
    } else if let Some(profile) = cli.profile {
        // Resolve profile name to path and set as HERMES_HOME
        let profile_path = hermes_core::hermes_home::resolve_profile_path(&profile);
        hermes_core::hermes_home::set_hermes_home(&profile_path)
            .ok();
    }

    let app = HermesApp::new()?;
    dispatch_command(&app, cli.command)
}
