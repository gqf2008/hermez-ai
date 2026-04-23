#![allow(dead_code)]
//! Version command — show detailed version information.

use console::Style;

fn cyan() -> Style { Style::new().cyan() }
fn dim() -> Style { Style::new().dim() }

/// Show version information.
pub fn cmd_version() {
    println!();
    println!("{}", cyan().apply_to("◆ Hermez Agent Version"));
    println!();
    println!("  Version: {}", env!("CARGO_PKG_VERSION"));
    println!("  Rust:    {}", dim().apply_to(&format!("{} ({} {})",
        env!("CARGO_PKG_RUST_VERSION"),
        option_env!("RUSTC_VERSION").unwrap_or("stable"),
        option_env!("RUSTC_HOST").unwrap_or("unknown"),
    )));
    println!("  Build:   {}", dim().apply_to(option_env!("VERGEN_GIT_SHA").unwrap_or("unknown")));
    println!();
}

/// Show version as a simple string (for scripts).
pub fn cmd_version_short() {
    println!("{}", env!("CARGO_PKG_VERSION"));
}
