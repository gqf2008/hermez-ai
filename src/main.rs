//! Hermez Agent CLI — main entry point.
//!
//! Replaces the Python `hermez` command (hermez_cli.main:main).
//! Supports subcommands: chat, setup, tools, skills, gateway, doctor, etc.
//!
//! Command schemas and dispatch logic have been extracted to `src/commands/`.

mod commands;

use clap::Parser;
use hermez_cli::app::HermezApp;
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

    // Set Hermez home if provided
    if let Some(home) = cli.hermez_home {
        hermez_core::hermez_home::set_hermez_home(&home)
            .ok();
    } else if let Some(profile) = cli.profile {
        // Resolve profile name to path and set as HERMEZ_HOME
        let profile_path = hermez_core::hermez_home::resolve_profile_path(&profile);
        hermez_core::hermez_home::set_hermez_home(&profile_path)
            .ok();
    }

    let app = HermezApp::new()?;
    dispatch_command(&app, cli.command)
}
