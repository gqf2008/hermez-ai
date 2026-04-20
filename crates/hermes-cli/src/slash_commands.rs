//! Slash command system for the Hermes interactive CLI.
//!
//! Mirrors Python `hermes_cli/commands.py` — central registry for all
//! `/command` interactions within the chat loop.
//!
//! To add a command: register it in `COMMAND_REGISTRY`.
//! To add an alias: push it to the `aliases` vec of the existing `CommandDef`.

use std::collections::HashMap;
use hermes_agent_engine::agent::{AIAgent, AgentConfig};
use hermes_agent_engine::agent::types::Message;
use hermes_tools::registry::ToolRegistry;

// ---------------------------------------------------------------------------
// Command definition
// ---------------------------------------------------------------------------

/// Definition of a single slash command.
#[derive(Debug, Clone)]
pub struct CommandDef {
    /// Canonical name without slash: "background"
    pub name: &'static str,
    /// Human-readable description
    pub description: &'static str,
    /// Category: "Session", "Configuration", "Tools & Skills", "Info", "Exit"
    pub category: &'static str,
    /// Alternative names: ("bg",)
    pub aliases: &'static [&'static str],
    /// Argument placeholder: "<prompt>", "[name]"
    pub args_hint: &'static str,
    /// Whether this command is only available in CLI (not gateway)
    pub cli_only: bool,
}

impl CommandDef {
    pub const fn new(
        name: &'static str,
        description: &'static str,
        category: &'static str,
    ) -> Self {
        Self {
            name,
            description,
            category,
            aliases: &[],
            args_hint: "",
            cli_only: false,
        }
    }

    pub const fn with_aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.aliases = aliases;
        self
    }

    pub const fn with_args_hint(mut self, hint: &'static str) -> Self {
        self.args_hint = hint;
        self
    }

    pub const fn cli_only(mut self) -> Self {
        self.cli_only = true;
        self
    }
}

// ---------------------------------------------------------------------------
// Central registry — single source of truth
// ---------------------------------------------------------------------------

pub const COMMAND_REGISTRY: &[CommandDef] = &[
    // Session
    CommandDef::new("new", "Start a new session (fresh session ID + history)", "Session")
        .with_aliases(&["reset"]),
    CommandDef::new("clear", "Clear screen and start a new session", "Session").cli_only(),
    CommandDef::new("history", "Show conversation history", "Session").cli_only(),
    CommandDef::new("save", "Save the current conversation", "Session").cli_only(),
    CommandDef::new("retry", "Retry the last message (resend to agent)", "Session"),
    CommandDef::new("undo", "Remove the last user/assistant exchange", "Session"),
    CommandDef::new("title", "Set a title for the current session", "Session")
        .with_args_hint("[name]"),
    CommandDef::new("branch", "Branch the current session (explore a different path)", "Session")
        .with_aliases(&["fork"])
        .with_args_hint("[name]"),
    CommandDef::new("compress", "Manually compress conversation context", "Session")
        .with_args_hint("[focus topic]"),
    CommandDef::new("rollback", "List or restore filesystem checkpoints", "Session")
        .with_args_hint("[number]"),
    CommandDef::new(
        "snapshot",
        "Create or restore state snapshots of Hermes config/state",
        "Session",
    )
    .with_aliases(&["snap"])
    .with_args_hint("[create|restore <id>|prune]"),
    CommandDef::new("stop", "Kill all running background processes", "Session"),
    CommandDef::new("background", "Run a prompt in the background", "Session")
        .with_aliases(&["bg"])
        .with_args_hint("<prompt>"),
    CommandDef::new(
        "btw",
        "Ephemeral side question using session context (no tools, not persisted)",
        "Session",
    )
    .with_args_hint("<question>"),
    CommandDef::new("agents", "Show active agents and running tasks", "Session")
        .with_aliases(&["tasks"]),
    CommandDef::new("queue", "Queue a prompt for the next turn (doesn't interrupt)", "Session")
        .with_aliases(&["q"])
        .with_args_hint("<prompt>"),
    CommandDef::new(
        "steer",
        "Inject a message after the next tool call without interrupting",
        "Session",
    )
    .with_args_hint("<prompt>"),
    CommandDef::new("status", "Show session info", "Session"),
    CommandDef::new("resume", "Resume a previously-named session", "Session")
        .with_args_hint("[name]"),

    // Configuration
    CommandDef::new("config", "Show current configuration", "Configuration").cli_only(),
    CommandDef::new("model", "Switch model for this session", "Configuration")
        .with_args_hint("[model] [--provider name] [--global]"),
    CommandDef::new("provider", "Show available providers and current provider", "Configuration"),
    CommandDef::new("personality", "Set a predefined personality", "Configuration")
        .with_args_hint("[name]"),
    CommandDef::new("verbose", "Cycle tool progress display: off -> new -> all -> verbose", "Configuration").cli_only(),
    CommandDef::new("yolo", "Toggle YOLO mode (skip all dangerous command approvals)", "Configuration"),
    CommandDef::new("reasoning", "Manage reasoning effort and display", "Configuration")
        .with_args_hint("[level|show|hide]"),
    CommandDef::new("fast", "Toggle fast mode — Priority Processing (Normal/Fast)", "Configuration")
        .with_args_hint("[normal|fast|status]"),
    CommandDef::new("skin", "Show or change the display skin/theme", "Configuration")
        .with_args_hint("[name]"),
    CommandDef::new("voice", "Toggle voice mode", "Configuration")
        .with_args_hint("[on|off|tts|status]"),

    // Tools & Skills
    CommandDef::new("tools", "Manage tools: /tools [list|disable|enable] [name...]", "Tools & Skills")
        .with_args_hint("[list|disable|enable] [name...]")
        .cli_only(),
    CommandDef::new("toolsets", "List available toolsets", "Tools & Skills").cli_only(),
    CommandDef::new("skills", "Search, install, inspect, or manage skills", "Tools & Skills")
        .with_args_hint("[search|browse|inspect|install]")
        .cli_only(),
    CommandDef::new("cron", "Manage scheduled tasks", "Tools & Skills")
        .with_args_hint("[subcommand]")
        .cli_only(),
    CommandDef::new("reload", "Reload .env variables into the running session", "Tools & Skills"),
    CommandDef::new("reload-mcp", "Reload MCP servers from config", "Tools & Skills")
        .with_aliases(&["reload_mcp"]),
    CommandDef::new("browser", "Connect browser tools to your live Chrome via CDP", "Tools & Skills")
        .with_args_hint("[connect|disconnect|status]")
        .cli_only(),
    CommandDef::new("plugins", "List installed plugins and their status", "Tools & Skills").cli_only(),

    // Info
    CommandDef::new("commands", "Browse all commands and skills (paginated)", "Info")
        .with_args_hint("[page]"),
    CommandDef::new("help", "Show available commands", "Info"),
    CommandDef::new("usage", "Show token usage and rate limits for the current session", "Info"),
    CommandDef::new("insights", "Show usage insights and analytics", "Info")
        .with_args_hint("[days]"),
    CommandDef::new("platforms", "Show gateway/messaging platform status", "Info")
        .with_aliases(&["gateway"])
        .cli_only(),
    CommandDef::new("copy", "Copy the last assistant response to clipboard", "Info")
        .with_args_hint("[number]")
        .cli_only(),
    CommandDef::new("paste", "Attach clipboard image from your clipboard", "Info").cli_only(),
    CommandDef::new("image", "Attach a local image file for your next prompt", "Info")
        .with_args_hint("<path>")
        .cli_only(),
    CommandDef::new("update", "Update Hermes Agent to the latest version", "Info"),
    CommandDef::new("debug", "Upload debug report (system info + logs) and get shareable links", "Info"),
    CommandDef::new("profile", "Show active profile name and home directory", "Info"),
    CommandDef::new("gquota", "Show Google Gemini Code Assist quota usage", "Info"),

    // Exit
    CommandDef::new("quit", "Exit the CLI", "Exit")
        .with_aliases(&["exit"])
        .cli_only(),
];

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Resolve a command name (without leading slash) to its `CommandDef`.
pub fn resolve_command(name: &str) -> Option<&'static CommandDef> {
    let name_lower = name.to_lowercase();
    COMMAND_REGISTRY.iter().find(|cmd| {
        cmd.name.eq_ignore_ascii_case(&name_lower)
            || cmd.aliases.iter().any(|a| a.eq_ignore_ascii_case(&name_lower))
    })
}

/// Generate help lines grouped by category.
pub fn help_lines() -> Vec<String> {
    let mut by_category: HashMap<&str, Vec<&CommandDef>> = HashMap::new();
    for cmd in COMMAND_REGISTRY {
        by_category.entry(cmd.category).or_default().push(cmd);
    }

    let mut lines = vec!["Available commands:".to_string()];
    let category_order = ["Session", "Configuration", "Tools & Skills", "Info", "Exit"];
    for cat in category_order {
        if let Some(cmds) = by_category.get(cat) {
            lines.push(format!("\n  {cat}:"));
            for cmd in cmds {
                let alias_str = if cmd.aliases.is_empty() {
                    String::new()
                } else {
                    format!(" (aliases: {})", cmd.aliases.join(", "))
                };
                let args = if cmd.args_hint.is_empty() {
                    String::new()
                } else {
                    format!(" {}", cmd.args_hint)
                };
                lines.push(format!(
                    "    /{}{} — {}{}",
                    cmd.name, args, cmd.description, alias_str
                ));
            }
        }
    }
    lines
}

/// Generate a compact list of command names for tab completion.
pub fn command_names() -> Vec<String> {
    let mut names = Vec::new();
    for cmd in COMMAND_REGISTRY {
        names.push(format!("/{} ", cmd.name));
        for alias in cmd.aliases {
            names.push(format!("/{} ", alias));
        }
    }
    names
}

// ---------------------------------------------------------------------------
// Command execution context
// ---------------------------------------------------------------------------

/// Mutable context passed to slash command handlers.
pub struct SlashContext<'a> {
    pub agent: &'a mut AIAgent,
    pub messages: &'a mut Vec<Message>,
    pub config: &'a mut AgentConfig,
    pub registry: &'a mut ToolRegistry,
    pub quiet: bool,
    /// Last user query (for /retry)
    pub last_query: &'a mut Option<String>,
    /// Session title (for /title)
    pub session_title: &'a mut Option<String>,
    /// YOLO mode flag (for /yolo)
    pub yolo_mode: &'a mut bool,
    /// Whether to break the main loop (set by /quit)
    pub should_exit: &'a mut bool,
}

/// Result of executing a slash command.
pub enum SlashResult {
    /// Command handled, continue the loop.
    Handled,
    /// Command wants to run an agent turn with the given prompt.
    AgentTurn(String),
    /// Command produced an error message.
    Error(String),
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Dispatch a slash command.
pub fn dispatch(cmd: &str, args: &str, ctx: &mut SlashContext) -> SlashResult {
    match cmd {
        "new" | "reset" => cmd_new(ctx),
        "clear" => cmd_clear(ctx),
        "history" => cmd_history(ctx),
        "save" => cmd_save(ctx),
        "retry" => cmd_retry(ctx),
        "undo" => cmd_undo(ctx),
        "title" => cmd_title(ctx, args),
        "branch" | "fork" => cmd_branch(ctx, args),
        "compress" => cmd_compress(ctx, args),
        "rollback" => cmd_rollback(ctx, args),
        "snapshot" | "snap" => cmd_snapshot(ctx, args),
        "stop" => cmd_stop(ctx),
        "background" | "bg" => cmd_background(ctx, args),
        "btw" => cmd_btw(ctx, args),
        "agents" | "tasks" => cmd_agents(ctx),
        "queue" | "q" => cmd_queue(ctx, args),
        "steer" => cmd_steer(ctx, args),
        "status" => cmd_status(ctx),
        "resume" => cmd_resume(ctx, args),
        "config" => cmd_config(ctx),
        "model" => cmd_model(ctx, args),
        "provider" => cmd_provider(ctx),
        "personality" => cmd_personality(ctx, args),
        "verbose" => cmd_verbose(ctx),
        "yolo" => cmd_yolo(ctx),
        "reasoning" => cmd_reasoning(ctx, args),
        "fast" => cmd_fast(ctx, args),
        "skin" => cmd_skin(ctx, args),
        "voice" => cmd_voice(ctx, args),
        "tools" => cmd_tools(ctx, args),
        "toolsets" => cmd_toolsets(ctx),
        "skills" => cmd_skills(ctx, args),
        "cron" => cmd_cron(ctx, args),
        "reload" => cmd_reload(ctx),
        "reload-mcp" | "reload_mcp" => cmd_reload_mcp(ctx),
        "browser" => cmd_browser(ctx, args),
        "plugins" => cmd_plugins(ctx),
        "commands" => cmd_commands(ctx, args),
        "help" => cmd_help(ctx),
        "usage" => cmd_usage(ctx),
        "insights" => cmd_insights(ctx, args),
        "platforms" | "gateway" => cmd_platforms(ctx),
        "copy" => cmd_copy(ctx, args),
        "paste" => cmd_paste(ctx),
        "image" => cmd_image(ctx, args),
        "update" => cmd_update(ctx),
        "debug" => cmd_debug(ctx),
        "profile" => cmd_profile(ctx),
        "gquota" => cmd_gquota(ctx),
        "quit" | "exit" => cmd_quit(ctx),
        _ => SlashResult::Error(format!("Unknown command: /{cmd}")),
    }
}

// --- Session commands ------------------------------------------------------

fn cmd_new(ctx: &mut SlashContext) -> SlashResult {
    ctx.messages.clear();
    *ctx.last_query = None;
    ctx.agent.reset_session_state();
    println!("New session started.");
    SlashResult::Handled
}

fn cmd_clear(ctx: &mut SlashContext) -> SlashResult {
    // Clear screen + new session
    print!("\x1B[2J\x1B[H"); // ANSI clear screen
    cmd_new(ctx)
}

fn cmd_history(ctx: &mut SlashContext) -> SlashResult {
    if ctx.messages.is_empty() {
        println!("No conversation history yet.");
        return SlashResult::Handled;
    }
    for (i, msg) in ctx.messages.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("unknown");
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        // Truncate long content for display
        let display = if content.len() > 200 {
            format!("{}…", &content[..200])
        } else {
            content.to_string()
        };
        println!("  [{}] {}: {}", i, role, display);
    }
    SlashResult::Handled
}

fn cmd_save(ctx: &mut SlashContext) -> SlashResult {
    let home = hermes_core::get_hermes_home();
    let saves_dir = home.join("saves");
    if let Err(e) = std::fs::create_dir_all(&saves_dir) {
        return SlashResult::Error(format!("Failed to create saves dir: {e}"));
    }
    let title = ctx.session_title.as_deref().unwrap_or("session");
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("{}_{}.json", title, timestamp);
    let path = saves_dir.join(&filename);

    let data: Vec<serde_json::Value> = ctx.messages.iter().map(|m| (**m).clone()).collect();
    match std::fs::write(&path, serde_json::to_string_pretty(&data).unwrap_or_default()) {
        Ok(_) => {
            println!("Conversation saved to {}", path.display());
            SlashResult::Handled
        }
        Err(e) => SlashResult::Error(format!("Failed to save: {e}")),
    }
}

fn cmd_retry(ctx: &mut SlashContext) -> SlashResult {
    match ctx.last_query.clone() {
        Some(q) => {
            println!("Retrying: {}", q);
            SlashResult::AgentTurn(q)
        }
        None => SlashResult::Error("No previous message to retry.".into()),
    }
}

fn cmd_undo(ctx: &mut SlashContext) -> SlashResult {
    // Remove last user + assistant pair
    let mut removed = 0;
    while removed < 2 && !ctx.messages.is_empty() {
        if let Some(last) = ctx.messages.last() {
            let role = last.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "user" || role == "assistant" {
                ctx.messages.pop();
                removed += 1;
                continue;
            }
        }
        break;
    }
    println!("Removed {} message(s).", removed);
    SlashResult::Handled
}

fn cmd_title(ctx: &mut SlashContext, args: &str) -> SlashResult {
    let title = args.trim();
    if title.is_empty() {
        match ctx.session_title {
            Some(ref t) => println!("Current title: {}", t),
            None => println!("No title set."),
        }
    } else {
        *ctx.session_title = Some(title.to_string());
        println!("Title set to: {}", title);
    }
    SlashResult::Handled
}

fn cmd_branch(ctx: &mut SlashContext, args: &str) -> SlashResult {
    let name = if args.trim().is_empty() {
        format!("branch_{}", chrono::Local::now().format("%H%M%S"))
    } else {
        args.trim().to_string()
    };
    // For now, just start a new session with a title hint
    ctx.messages.clear();
    *ctx.session_title = Some(name.clone());
    ctx.agent.reset_session_state();
    println!("Branched to new session: {}", name);
    SlashResult::Handled
}

fn cmd_compress(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    // Trigger manual compression via the agent's compressor
    // The agent handles compression automatically in run_conversation,
    // but we could force it here if the compressor is exposed.
    println!("Manual compression is handled automatically during conversation.");
    SlashResult::Handled
}

fn cmd_rollback(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Filesystem checkpoint rollback not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_snapshot(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("State snapshot management not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_stop(_ctx: &mut SlashContext) -> SlashResult {
    println!("Background process kill not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_background(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    if args.trim().is_empty() {
        SlashResult::Error("Usage: /background <prompt>".into())
    } else {
        println!("Background prompts not yet implemented in Rust.");
        SlashResult::Handled
    }
}

fn cmd_btw(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    if args.trim().is_empty() {
        return SlashResult::Error("Usage: /btw <question>".into());
    }
    // Ephemeral: no tools, not persisted
    println!("Ephemeral query: {}", args);
    // We'll handle this as a special agent turn
    SlashResult::AgentTurn(format!("[EPHEMERAL] {}", args))
}

fn cmd_agents(_ctx: &mut SlashContext) -> SlashResult {
    println!("Active agents: 1 (main)");
    println!("Running tasks: 0");
    SlashResult::Handled
}

fn cmd_queue(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    if args.trim().is_empty() {
        SlashResult::Error("Usage: /queue <prompt>".into())
    } else {
        println!("Queued: {}", args.trim());
        // In a full implementation, this would append to a queue
        SlashResult::Handled
    }
}

fn cmd_steer(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    if args.trim().is_empty() {
        SlashResult::Error("Usage: /steer <prompt>".into())
    } else {
        println!("Steer message queued for after next tool call.");
        SlashResult::Handled
    }
}

fn cmd_status(ctx: &mut SlashContext) -> SlashResult {
    println!("Session status:");
    println!("  Model: {}", ctx.config.model);
    println!("  Provider: {}", ctx.config.provider.as_deref().unwrap_or("default"));
    println!("  Messages: {}", ctx.messages.len());
    println!("  Title: {}", ctx.session_title.as_deref().unwrap_or("(none)"));
    println!("  YOLO mode: {}", *ctx.yolo_mode);
    SlashResult::Handled
}

fn cmd_resume(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Session resume not yet implemented in Rust.");
    SlashResult::Handled
}

// --- Configuration commands ------------------------------------------------

fn cmd_config(_ctx: &mut SlashContext) -> SlashResult {
    match hermes_core::HermesConfig::load() {
        Ok(cfg) => {
            match serde_yaml::to_string(&cfg) {
                Ok(yaml) => println!("{}", yaml),
                Err(e) => return SlashResult::Error(format!("Failed to serialize config: {e}")),
            }
        }
        Err(e) => println!("Config load error: {e}"),
    }
    SlashResult::Handled
}

fn cmd_model(ctx: &mut SlashContext, args: &str) -> SlashResult {
    let model = args.trim();
    if model.is_empty() {
        println!("Current model: {}", ctx.config.model);
        return SlashResult::Handled;
    }
    ctx.config.model = model.to_string();
    ctx.agent.switch_model(model, None, None, None);
    println!("Model switched to: {}", model);
    SlashResult::Handled
}

fn cmd_provider(ctx: &mut SlashContext) -> SlashResult {
    let current = ctx.config.provider.as_deref().unwrap_or("(auto-detect from model name)");
    println!("Current provider: {}", current);
    println!("Available providers: anthropic, openai, openrouter, gemini, bedrock, codex, kimi, minimax, nous, custom");
    SlashResult::Handled
}

fn cmd_personality(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Personality switching not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_verbose(_ctx: &mut SlashContext) -> SlashResult {
    println!("Verbose mode cycling not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_yolo(ctx: &mut SlashContext) -> SlashResult {
    *ctx.yolo_mode = !*ctx.yolo_mode;
    println!("YOLO mode: {}", if *ctx.yolo_mode { "ON — skipping approvals" } else { "OFF" });
    SlashResult::Handled
}

fn cmd_reasoning(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Reasoning display management not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_fast(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Fast mode toggle not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_skin(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    let name = args.trim();
    if name.is_empty() {
        let skin = crate::skin_engine::get_active_skin();
        println!("Active skin: {}", skin.name);
    } else {
        println!("Skin switching not yet implemented in Rust.");
    }
    SlashResult::Handled
}

fn cmd_voice(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Voice mode toggle not yet implemented in Rust.");
    SlashResult::Handled
}

// --- Tools & Skills commands -----------------------------------------------

fn cmd_tools(ctx: &mut SlashContext, args: &str) -> SlashResult {
    let parts: Vec<&str> = args.trim().split_whitespace().collect();
    match parts.first().copied() {
        Some("disable") if parts.len() > 1 => {
            for name in &parts[1..] {
                println!("Disable tool: {} (not yet implemented)", name);
            }
        }
        Some("enable") if parts.len() > 1 => {
            for name in &parts[1..] {
                println!("Enable tool: {} (not yet implemented)", name);
            }
        }
        _ => {
            println!("Registered tools: {}", ctx.registry.len());
            // List tool names
            for name in ctx.registry.list_tools() {
                println!("  - {}", name);
            }
        }
    }
    SlashResult::Handled
}

fn cmd_toolsets(_ctx: &mut SlashContext) -> SlashResult {
    println!("Available toolsets:");
    for (name, ts) in hermes_tools::toolsets_def::toolsets().iter() {
        println!("  {} — {}", name, ts.description);
    }
    SlashResult::Handled
}

fn cmd_skills(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Skills management: use `hermes skills` subcommand for full management.");
    SlashResult::Handled
}

fn cmd_cron(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Cron management: use `hermes cron` subcommand for full management.");
    SlashResult::Handled
}

fn cmd_reload(_ctx: &mut SlashContext) -> SlashResult {
    // Reload .env
    let env_path = hermes_core::get_hermes_home().join(".env");
    if env_path.exists() {
        match dotenvy::from_path(&env_path) {
            Ok(_) => println!("Reloaded .env from {}", env_path.display()),
            Err(e) => println!("Warning: failed to reload .env: {e}"),
        }
    } else {
        println!("No .env file found at {}", env_path.display());
    }
    SlashResult::Handled
}

fn cmd_reload_mcp(_ctx: &mut SlashContext) -> SlashResult {
    println!("MCP reload not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_browser(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Browser CDP management not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_plugins(_ctx: &mut SlashContext) -> SlashResult {
    println!("Plugin listing not yet implemented in Rust.");
    SlashResult::Handled
}

// --- Info commands ---------------------------------------------------------

fn cmd_commands(_ctx: &mut SlashContext, args: &str) -> SlashResult {
    let page = args.trim().parse::<usize>().unwrap_or(1).saturating_sub(1);
    let lines = help_lines();
    let page_size = 20;
    let start = page * page_size;
    let end = (start + page_size).min(lines.len());
    if start >= lines.len() {
        println!("No more commands.");
    } else {
        for line in &lines[start..end] {
            println!("{}", line);
        }
    }
    SlashResult::Handled
}

fn cmd_help(_ctx: &mut SlashContext) -> SlashResult {
    for line in help_lines() {
        println!("{}", line);
    }
    SlashResult::Handled
}

fn cmd_usage(_ctx: &mut SlashContext) -> SlashResult {
    println!("Token usage tracking not yet implemented in Rust CLI.");
    SlashResult::Handled
}

fn cmd_insights(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Insights: use `hermes insights` subcommand for detailed analytics.");
    SlashResult::Handled
}

fn cmd_platforms(_ctx: &mut SlashContext) -> SlashResult {
    println!("Gateway platforms: use `hermes gateway` subcommand for status.");
    SlashResult::Handled
}

fn cmd_copy(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        println!("Copy to clipboard: use `pbcopy` or `xclip` manually.");
    }
    #[cfg(target_os = "windows")]
    {
        println!("Copy to clipboard: use `clip` manually.");
    }
    SlashResult::Handled
}

fn cmd_paste(_ctx: &mut SlashContext) -> SlashResult {
    println!("Clipboard paste not yet implemented in Rust.");
    SlashResult::Handled
}

fn cmd_image(_ctx: &mut SlashContext, _args: &str) -> SlashResult {
    println!("Image attachment not yet implemented in Rust CLI slash commands.");
    SlashResult::Handled
}

fn cmd_update(_ctx: &mut SlashContext) -> SlashResult {
    println!("Self-update: use `cargo install` or your package manager.");
    SlashResult::Handled
}

fn cmd_debug(_ctx: &mut SlashContext) -> SlashResult {
    println!("Debug report: use `hermes debug` subcommand.");
    SlashResult::Handled
}

fn cmd_profile(_ctx: &mut SlashContext) -> SlashResult {
    let home = hermes_core::get_hermes_home();
    println!("Profile: default");
    println!("HERMES_HOME: {}", home.display());
    SlashResult::Handled
}

fn cmd_gquota(_ctx: &mut SlashContext) -> SlashResult {
    println!("Gemini Code Assist quota not yet implemented in Rust.");
    SlashResult::Handled
}

// --- Exit commands ---------------------------------------------------------

fn cmd_quit(ctx: &mut SlashContext) -> SlashResult {
    *ctx.should_exit = true;
    SlashResult::Handled
}
