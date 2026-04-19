#![allow(clippy::too_many_arguments, clippy::result_large_err, dead_code)]
//! # Hermes CLI
//!
//! CLI application with TUI, subcommands, and setup wizard.
//! Mirrors the Python `hermes_cli/` directory.

use std::io::{self, Write};

/// Prompt the user for a yes/no confirmation.
pub fn confirm(prompt: &str) -> io::Result<bool> {
    print!("{prompt} [y/N]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim().to_lowercase();
    Ok(input == "y" || input == "yes")
}

pub mod acp_cmd;
pub mod app;
pub mod auth_cmd;
pub mod backup_cmd;
pub mod banner;
pub mod batch_cmd;
pub mod clipboard;
pub mod claw_cmd;
pub mod copilot_auth;
pub mod completion_cmd;
pub mod config_cmd;
pub mod cron_cmd;
pub mod dashboard_cmd;
pub mod debug_cmd;
pub mod debug_share_cmd;
pub mod display;
pub mod doctor_cmd;
pub mod dump_cmd;
pub mod gateway_mgmt;
pub mod insights_cmd;
pub mod login_cmd;
pub mod logs_cmd;
pub mod oauth_flow;
pub mod oauth_server;
pub mod oauth_store;
pub mod mcp_cmd;
pub mod memory_cmd;
pub mod model_cmd;
pub mod nous_subscription;
pub mod pairing_cmd;
pub mod plugins_cmd;
pub mod profiles_cmd;
pub mod sessions_cmd;
pub mod setup_cmd;
pub mod skin_engine;
pub mod skills_hub_cmd;
pub mod status_cmd;
pub mod tools_cmd;
pub mod uninstall_cmd;
pub mod update_cmd;
pub mod version_cmd;
pub mod webhook_cmd;
pub mod whatsapp_cmd;

pub mod tips;
pub mod tui;
