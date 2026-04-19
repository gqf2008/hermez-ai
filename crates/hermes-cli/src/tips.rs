#![allow(dead_code)]
//! Random tips shown at CLI session start to help users discover features.
//!
//! Mirrors Python `hermes_cli/tips.py`.

use rand::seq::{IndexedRandom, SliceRandom};

/// The tip corpus — one-liners covering slash commands, CLI flags, config,
/// keybindings, tools, gateway, skills, profiles, and workflow tricks.
const TIPS: &[&str] = &[
    // Slash Commands
    "/btw <question> asks a quick side question without tools or history — great for clarifications.",
    "/background <prompt> runs a task in a separate session while your current one stays free.",
    "/branch forks the current session so you can explore a different direction without losing progress.",
    "/compress manually compresses conversation context when things get long.",
    "/rollback lists filesystem checkpoints — restore files the agent modified to any prior state.",
    "/rollback diff 2 previews what changed since checkpoint 2 without restoring anything.",
    "/rollback 2 src/file.py restores a single file from a specific checkpoint.",
    "/title \"my project\" names your session — resume it later with /resume or hermes -c.",
    "/resume picks up where you left off in a previously named session.",
    "/queue <prompt> queues a message for the next turn without interrupting the current one.",
    "/undo removes the last user/assistant exchange from the conversation.",
    "/retry resends your last message — useful when the agent's response wasn't quite right.",
    "/verbose cycles tool progress display: off → new → all → verbose.",
    "/reasoning high increases the model's thinking depth. /reasoning show displays the reasoning.",
    "/fast toggles priority processing for faster API responses (provider-dependent).",
    "/yolo skips all dangerous command approval prompts for the rest of the session.",
    "/model lets you switch models mid-session — try /model sonnet or /model gpt-5.",
    "/model --global changes your default model permanently.",
    "/personality pirate sets a fun personality — 14 built-in options from kawaii to shakespeare.",
    "/skin changes the CLI theme — try ares, mono, slate, poseidon, or charizard.",
    "/statusbar toggles a persistent bar showing model, tokens, context fill %, cost, and duration.",
    "/tools disable browser temporarily removes browser tools for the current session.",
    "/browser connect attaches browser tools to your running Chrome instance via CDP.",
    "/plugins lists installed plugins and their status.",
    "/cron manages scheduled tasks — set up recurring prompts with delivery to any platform.",
    "/reload-mcp hot-reloads MCP server configuration without restarting.",
    "/usage shows token usage, cost breakdown, and session duration.",
    "/insights shows usage analytics for the last 30 days.",
    "/paste checks your clipboard for an image and attaches it to your next message.",
    "/profile shows which profile is active and its home directory.",
    "/config shows your current configuration at a glance.",
    "/stop kills all running background processes spawned by the agent.",

    // @ Context References
    "@file:path/to/file.py injects file contents directly into your message.",
    "@file:main.py:10-50 injects only lines 10-50 of a file.",
    "@folder:src/ injects a directory tree listing.",
    "@diff injects your unstaged git changes into the message.",
    "@staged injects your staged git changes (git diff --staged).",
    "@git:5 injects the last 5 commits with full patches.",
    "@url:https://example.com fetches and injects a web page's content.",
    "Typing @ triggers filesystem path completion — navigate to any file interactively.",
    "Combine multiple references: \"Review @file:main.py and @file:test.py for consistency.\"",

    // Keybindings
    "Alt+Enter (or Ctrl+J) inserts a newline for multi-line input.",
    "Ctrl+C interrupts the agent. Double-press within 2 seconds to force exit.",
    "Ctrl+Z suspends Hermes to the background — run fg in your shell to resume.",
    "Tab accepts auto-suggestion ghost text or autocompletes slash commands.",
    "Type a new message while the agent is working to interrupt and redirect it.",
    "Alt+V pastes an image from your clipboard into the conversation.",
    "Pasting 5+ lines auto-saves to a file and inserts a compact reference instead.",

    // CLI Flags
    "hermes -c resumes your most recent CLI session. hermes -c \"project name\" resumes by title.",
    "hermes -w creates an isolated git worktree — perfect for parallel agent workflows.",
    "hermes -w -q \"Fix issue #42\" combines worktree isolation with a one-shot query.",
    "hermes chat -t web,terminal enables only specific toolsets for a focused session.",
    "hermes chat -s github-pr-workflow preloads a skill at launch.",
    "hermes chat -q \"query\" runs a single non-interactive query and exits.",
    "hermes chat --max-turns 200 overrides the default 90-iteration limit per turn.",
    "hermes chat --checkpoints enables filesystem snapshots before every destructive file change.",
    "hermes --yolo bypasses all dangerous command approval prompts for the entire session.",
    "hermes chat --source telegram tags the session for filtering in hermes sessions list.",
    "hermes -p work chat runs under a specific profile without changing your default.",

    // CLI Subcommands
    "hermes doctor --fix diagnoses and auto-repairs config and dependency issues.",
    "hermes dump saves a compressed archive of your session for sharing or debugging.",
    "hermes sessions list shows all past sessions — filter with --since, --platform, or --query.",
    "hermes sessions export 42 > session.json exports a single session to JSON.",
    "hermes skills list shows installed skills. hermes skills install <url> adds a new one.",
    "hermes tools list shows all available tools and their allowlist status.",
    "hermes tools approve terminal permanently allows the terminal tool without prompting.",
    "hermes auth add openrouter --key <key> adds a credential to the rotation pool.",
    "hermes auth list shows all pooled credentials with masked keys.",
    "hermes auth reset openrouter clears exhaustion flags for a provider's credentials.",
    "hermes profile create work creates an isolated profile with its own config and history.",
    "hermes profile export work > work.zip exports a profile for backup or transfer.",
    "hermes cron list shows scheduled jobs. hermes cron add \"0 9 * * *\" \"morning standup\" creates one.",
    "hermes gateway start launches the messaging gateway for Telegram, Discord, Slack, etc.",
    "hermes mcp list shows configured MCP servers. hermes mcp add <name> <url> registers one.",

    // Config
    "Set model.default in ~/.hermes/config.yaml to change your default model.",
    "Set model.provider to auto and Hermes will pick the best available provider at runtime.",
    "Add custom_providers to config.yaml for private endpoints (Ollama, vLLM, etc.).",
    "Set display.show_thinking: true to see the model's reasoning process in real time.",
    "Set display.spinner: false to disable the animated spinner during API calls.",
    "Set memory.enabled: false to disable persistent memory across sessions.",
    "Set tools.terminal.auto_allow: true to skip approval for all terminal commands.",
    "Set compression.enabled: true to automatically compress long conversations.",

    // Tools & Workflow
    "The terminal tool supports Docker, SSH, and local shells — set terminal.backend in config.",
    "Use @folder:docs/ to inject an entire directory tree for high-level architecture questions.",
    "The browser tool can connect to your existing Chrome via CDP — no headless overhead.",
    "Skills are reusable prompt templates — create one with /skill save <name> during any session.",
    "The delegate tool spawns a subagent for parallel task execution with isolated context.",
    "Memory entries are searchable — ask the agent \"What did we decide about X last week?\"",
    "The web_search tool supports Google, Bing, and DuckDuckGo — set search.engine in config.",
    "Use /stop to kill all background processes if the agent spawned something runaway.",

    // Gateway
    "The gateway supports 15+ platforms from a single process — add platforms in config.yaml.",
    "Gateway sessions are saved to SQLite and searchable with hermes sessions list.",
    "Set gateway.default_model to use a different model for messaging than the CLI default.",
    "Gateway messages can trigger the same tools as the CLI — code execution, web search, etc.",

    // Advanced
    "Subagents inherit the parent's tool registry but get their own isolated message history.",
    "Set HERMES_HOME to a different directory for completely isolated agent instances.",
    "The credential pool rotates API keys on 401/429 errors — add multiple keys for reliability.",
    "Use hermes chat --quiet for scripting — only the final response is printed to stdout.",
    "Environment variables in config.yaml are expanded at load time: ${OPENROUTER_API_KEY}.",
    "The agent detects and strips Unicode surrogate characters that can break some APIs.",
];

/// Return a random tip from the corpus.
///
/// Mirrors Python `random.choice(TIPS)`.
pub fn random_tip() -> &'static str {
    let mut rng = rand::rng();
    TIPS.choose(&mut rng).unwrap_or(&"Type /help to see all available commands.")
}

/// Return `n` random tips, deduplicated.
///
/// If `n` exceeds the corpus size, returns all tips in random order.
pub fn random_tips(n: usize) -> Vec<&'static str> {
    let mut rng = rand::rng();
    let mut tips: Vec<_> = TIPS.to_vec();
    tips.shuffle(&mut rng);
    tips.into_iter().take(n.min(TIPS.len())).collect()
}

/// Print a single random tip to stdout with decorative framing.
pub fn print_tip() {
    let dim = console::Style::new().dim();
    let tip = random_tip();
    println!("  {} {}", dim.apply_to("Tip:"), tip);
}

/// Print `n` random tips.
pub fn print_tips(n: usize) {
    let dim = console::Style::new().dim();
    for tip in random_tips(n) {
        println!("  {} {}", dim.apply_to("Tip:"), tip);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_random_tip_returns_something() {
        let tip = random_tip();
        assert!(!tip.is_empty());
    }

    #[test]
    fn test_random_tips_count() {
        let tips = random_tips(3);
        assert_eq!(tips.len(), 3);
        // Should be deduplicated
        let unique: std::collections::HashSet<_> = tips.iter().collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn test_random_tips_capped() {
        let tips = random_tips(10000);
        assert_eq!(tips.len(), TIPS.len());
    }

    #[test]
    fn test_corpus_not_empty() {
        assert!(!TIPS.is_empty());
    }
}
