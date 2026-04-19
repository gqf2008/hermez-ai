//! Hermes Agent — core conversation loop standalone binary.
//!
//! Replaces the Python `hermes-agent` command (run_agent:main).
//! Runs a single agent conversation from stdin to stdout.

use std::sync::Arc;

use clap::Parser;
use hermes_agent_engine::agent::{AIAgent, AgentConfig};
use hermes_tools::registry::ToolRegistry;
use hermes_tools::register_all_tools;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "hermes-agent", about = "Hermes Agent core loop", version)]
struct Cli {
    /// Model to use
    #[arg(short, long, default_value = "anthropic/claude-opus-4.6")]
    model: String,

    /// Maximum number of iterations
    #[arg(short = 'n', long, default_value_t = 90)]
    max_iterations: usize,

    /// Comma-separated list of enabled toolsets
    #[arg(long)]
    enabled_toolsets: Option<String>,

    /// Comma-separated list of disabled toolsets
    #[arg(long)]
    disabled_toolsets: Option<String>,

    /// Quiet mode
    #[arg(short, long)]
    quiet: bool,

    /// Save trajectories
    #[arg(long)]
    save_trajectories: bool,

    /// Skip loading context files
    #[arg(long)]
    skip_context_files: bool,

    /// Skip memory loading
    #[arg(long)]
    skip_memory: bool,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    if cli.verbose {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("debug"))
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::new("info"))
            .init();
    }

    if !cli.quiet {
        println!("Hermes Agent v{}", env!("CARGO_PKG_VERSION"));
        println!("Model: {}", cli.model);
        println!("Max iterations: {}", cli.max_iterations);
        println!();
    }

    // Build tool registry
    let mut registry = ToolRegistry::new();
    register_all_tools(&mut registry);

    if !cli.quiet {
        println!("Registered {} tools", registry.len());
        println!();
    }

    // Filter toolsets if specified
    if let Some(ref disabled) = cli.disabled_toolsets {
        let disabled_set: Vec<String> = disabled.split(',').map(|s| s.trim().to_string()).collect();
        // Tools are filtered via get_definitions based on availability checks
        tracing::info!("Disabled toolsets: {:?}", disabled_set);
    }

    // Build agent config
    let config = AgentConfig {
        model: cli.model.clone(),
        max_iterations: cli.max_iterations,
        skip_context_files: cli.skip_context_files,
        ..AgentConfig::default()
    };

    // Create agent
    let mut agent = AIAgent::new(config, Arc::new(registry))?;

    // Read user message from stdin (or first argument)
    let args: Vec<String> = std::env::args().skip(1).collect();
    let user_message = if args.is_empty() {
        // Interactive mode: read from stdin
        if !cli.quiet {
            print!("> ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        input.trim().to_string()
    } else {
        // Non-interactive: join remaining args
        args.join(" ")
    };

    if user_message.is_empty() {
        println!("No input provided. Exiting.");
        return Ok(());
    }

    if !cli.quiet {
        println!("Starting conversation...");
        println!();
    }

    // Run the conversation
    let turn_result = agent.run_conversation(&user_message, None, None).await;

    // Output result
    if !cli.quiet {
        println!("---");
        println!("Exit reason: {}", turn_result.exit_reason);
        println!("API calls: {}", turn_result.api_calls);
        println!("Messages in history: {}", turn_result.messages.len());
        println!();
    }

    // Print the final response
    if !turn_result.response.is_empty() {
        println!("{}", turn_result.response);
    } else {
        // If no text response, show the last assistant message
        for msg in turn_result.messages.iter().rev() {
            if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
                if role == "assistant" {
                    if let Some(content) = msg.get("content").and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            println!("{}", content);
                            break;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
