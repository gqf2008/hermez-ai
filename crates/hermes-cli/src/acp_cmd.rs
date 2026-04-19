#![allow(dead_code)]
//! ACP (Agent Client Protocol) command.
//!
//! Mirrors Python: hermes acp (IDE integration for VS Code, Zed, JetBrains)

use console::Style;

fn cyan() -> Style { Style::new().cyan() }
fn green() -> Style { Style::new().green() }
fn yellow() -> Style { Style::new().yellow() }
fn dim() -> Style { Style::new().dim() }

/// Show ACP status and configuration.
pub fn cmd_acp(action: &str, editor: Option<&str>) -> anyhow::Result<()> {
    println!();
    println!("{}", cyan().apply_to("◆ ACP (Agent Client Protocol)"));
    println!();

    match action {
        "status" => {
            println!("  {}", dim().apply_to("Supported IDEs:"));
            println!("    - VS Code (via extension)");
            println!("    - Zed (via native protocol)");
            println!("    - JetBrains (via plugin)");
            println!();
            println!("  {}", dim().apply_to("Run `hermes acp install <editor>` to set up IDE integration."));
        }
        "install" => {
            let ed = editor.ok_or_else(|| anyhow::anyhow!("editor name required (vscode, zed, jetbrains)"))?;
            match ed.to_lowercase().as_str() {
                "vscode" | "vs-code" | "vs_code" => {
                    println!("  Installing VS Code extension...");
                    println!("  {}", dim().apply_to("Install the Hermes extension from the VS Code Marketplace."));
                    println!("  {}", dim().apply_to("Search for 'Hermes Agent' in the Extensions view."));
                }
                "zed" => {
                    println!("  Installing Zed integration...");
                    println!("  {}", dim().apply_to("Add to your Zed settings.json:"));
                    println!();
                    println!("    {{");
                    println!("      \"language_servers\": [\"hermes\"]");
                    println!("    }}");
                }
                "jetbrains" | "idea" | "webstorm" | "pycharm" => {
                    println!("  Installing JetBrains plugin...");
                    println!("  {}", dim().apply_to("Install the Hermes plugin from the JetBrains Plugin Marketplace."));
                }
                _ => {
                    println!("  {} Unknown editor: {ed}", yellow().apply_to("⚠"));
                    println!("  Supported: vscode, zed, jetbrains");
                }
            }
        }
        "run" => {
            // Delegate to the hermes_acp binary for the actual ACP server
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("hermes_acp")))
                .filter(|p| p.exists());

            if let Some(acp_path) = exe {
                println!("  Starting ACP server at {}...", acp_path.display());
                let status = std::process::Command::new(&acp_path)
                    .stdin(std::process::Stdio::inherit())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .status();

                match status {
                    Ok(s) if s.success() => {
                        println!("  {} ACP server exited normally.", green().apply_to("✓"));
                    }
                    Ok(s) => {
                        println!("  {} ACP server exited with code: {}", yellow().apply_to("⚠"), s);
                    }
                    Err(e) => {
                        println!("  {} Failed to start ACP server: {}", yellow().apply_to("⚠"), e);
                    }
                }
            } else {
                println!("  {}", yellow().apply_to("⚠ hermes_acp binary not found."));
                println!("  {}", dim().apply_to("Build it with: cargo build --bin hermes_acp"));
            }
        }
        _ => {
            println!("  {}", dim().apply_to("Usage: hermes acp <status|install|run>"));
        }
    }
    println!();

    Ok(())
}
