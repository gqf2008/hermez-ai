#![allow(dead_code)]
//! System prompt assembly — identity, platform hints, skills index, context files.
//!
//! Mirrors the Python `agent/prompt_builder.py`.
//! All functions are stateless.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::injection_scan::sanitize_context_content;
use crate::skills_prompt::build_skills_system_prompt;
use crate::soul::{load_soul_md, CONTEXT_FILE_MAX_CHARS as SOUL_CONTEXT_FILE_MAX_CHARS};

// Re-export from soul module
pub use crate::soul::DEFAULT_AGENT_IDENTITY;

// --- Constants ---

/// Tool-use enforcement guidance for GPT/Codex/Gemini/etc.
pub const TOOL_USE_ENFORCEMENT_GUIDANCE: &str =
    "# Tool-use enforcement\n\
    You MUST use your tools to take action — do not describe what you would do \
    or plan to do without actually doing it. When you say you will perform an \
    action (e.g. 'I will run the tests', 'Let me check the file', 'I will create \
    the project'), you MUST immediately make the corresponding tool call in the same \
    response. Never end your turn with a promise of future action — execute it now.\n\
    Keep working until the task is actually complete. Do not stop with a summary of \
    what you plan to do next time. If you have tools available that can accomplish \
    the task, use them instead of telling the user what you would do.\n\
    Every response should either (a) contain tool calls that make progress, or \
    (b) deliver a final result to the user. Responses that only describe intentions \
    without acting are not acceptable.";

/// Memory guidance — injected when memory tool is available.
pub const MEMORY_GUIDANCE: &str =
    "You have persistent memory across sessions. Save durable facts using the memory \
    tool: user preferences, environment details, tool quirks, and stable conventions. \
    Memory is injected into every turn, so keep it compact and focused on facts that \
    will still matter later.\n\
    Prioritize what reduces future user steering — the most valuable memory is one \
    that prevents the user from having to correct or remind you again. \
    User preferences and recurring corrections matter more than procedural task details.\n\
    Do NOT save task progress, session outcomes, completed-work logs, or temporary TODO \
    state to memory; use session_search to recall those from past transcripts. \
    If you've discovered a new way to do something, solved a problem that could be \
    necessary later, save it as a skill with the skill tool.";

/// Session search guidance — injected when session_search tool is available.
pub const SESSION_SEARCH_GUIDANCE: &str =
    "When the user references something from a past conversation or you suspect \
    relevant cross-session context exists, use session_search to recall it before \
    asking them to repeat themselves.";

/// Skills guidance — injected when skill_manage tool is available.
pub const SKILLS_GUIDANCE: &str =
    "After completing a complex task (5+ tool calls), fixing a tricky error, \
    or discovering a non-trivial workflow, save the approach as a \
    skill with skill_manage so you can reuse it next time.\n\
    When using a skill and finding it outdated, incomplete, or wrong, \
    patch it immediately with skill_manage(action='patch') — don't wait to be asked. \
    Skills that aren't maintained become liabilities.";

/// Model name substrings that trigger tool-use enforcement.
pub const TOOL_USE_ENFORCEMENT_MODELS: &[&str] = &["gpt", "codex", "gemini", "gemma", "grok"];

/// OpenAI GPT/Codex-specific execution guidance.
pub const OPENAI_MODEL_EXECUTION_GUIDANCE: &str =
    "# Execution discipline\n\
    <tool_persistence>\n\
    - Use tools whenever they improve correctness, completeness, or grounding.\n\
    - Do not stop early when another tool call would materially improve the result.\n\
    - If a tool returns empty or partial results, retry with a different query or \
    strategy before giving up.\n\
    - Keep calling tools until: (1) the task is complete, AND (2) you have verified \
    the result.\n\
    </tool_persistence>\n\n\
    <mandatory_tool_use>\n\
    NEVER answer these from memory or mental computation — ALWAYS use a tool:\n\
    - Arithmetic, math, calculations → use terminal or execute_code\n\
    - Hashes, encodings, checksums → use terminal\n\
    - Current time, date, timezone → use terminal\n\
    - System state: OS, CPU, memory, disk, ports, processes → use terminal\n\
    - File contents, sizes, line counts → use file tools\n\
    - Git history, branches, diffs → use terminal\n\
    - Current facts (weather, news, versions) → use web_search\n\
    </mandatory_tool_use>\n\n\
    <verification>\n\
    Before finalizing your response:\n\
    - Correctness: does the output satisfy every stated requirement?\n\
    - Grounding: are factual claims backed by tool outputs or provided context?\n\
    - Formatting: does the output match the requested format or schema?\n\
    </verification>";

/// Gemini/Gemma-specific operational guidance.
pub const GOOGLE_MODEL_OPERATIONAL_GUIDANCE: &str =
    "# Google model operational directives\n\
    Follow these operational rules strictly:\n\
    - **Absolute paths:** Always construct and use absolute file paths.\n\
    - **Verify first:** Use file tools to check contents before making changes.\n\
    - **Dependency checks:** Never assume a library is available. Check config files first.\n\
    - **Conciseness:** Keep explanatory text brief — a few sentences, not paragraphs.\n\
    - **Parallel tool calls:** When you need multiple independent operations, call them together.\n\
    - **Non-interactive commands:** Use -y, --yes, --non-interactive flags.\n\
    - **Keep going:** Work autonomously until the task is fully resolved.";

/// Platform hints for gateway mode.
pub fn platform_hints() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "whatsapp",
            "You are on a text messaging communication platform, WhatsApp. \
            Please do not use markdown as it does not render.",
        ),
        (
            "telegram",
            "You are on a text messaging communication platform, Telegram. \
            Please do not use markdown as it does not render.",
        ),
        (
            "discord",
            "You are in a Discord server or group chat communicating with your user.",
        ),
        (
            "slack",
            "You are in a Slack workspace communicating with your user.",
        ),
        (
            "cli",
            "You are a CLI AI Agent. Try not to use markdown but simple text \
            renderable inside a terminal.",
        ),
    ]
}

/// Check if running inside WSL (Windows Subsystem for Linux).
///
/// Mirrors Python: checks `/proc/sys/kernel/osrelease` for "microsoft" or "WSL".
#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    if let Ok(content) = std::fs::read_to_string("/proc/sys/kernel/osrelease") {
        return content.contains("microsoft") || content.contains("WSL");
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn is_wsl() -> bool {
    // On Windows, check for WSL environment variable
    std::env::var("WSL_DISTRO_NAME").is_ok()
}

/// Build environment-specific hints (WSL, Termux, Docker, etc.).
///
/// Returns None if no environment hints apply.
fn build_environment_hints() -> Option<String> {
    // WSL detection
    if is_wsl() {
        return Some(
            "## Environment\n\
             You are running in WSL (Windows Subsystem for Linux). \
             Windows filesystem paths are available at /mnt/c/, /mnt/d/, etc. \
             When the user refers to Windows paths like C:\\Users\\..., \
             use /mnt/c/Users/... instead."
                .to_string(),
        );
    }
    // Extend here for Termux, Docker, etc.
    None
}

/// Context file priority names for .hermez.md search.
const HERMEZ_MD_NAMES: &[&str] = &[".hermez.md", "HERMEZ.md"];

/// Truncation ratios.
const TRUNCATE_HEAD_RATIO: f64 = 0.7;
const TRUNCATE_TAIL_RATIO: f64 = 0.2;

// --- System Prompt Builder ---

/// Configuration for the prompt builder.
#[derive(Debug, Clone, Default)]
pub struct PromptBuilderConfig {
    /// Current model name.
    pub model: Option<String>,
    /// Current provider.
    pub provider: Option<String>,
    /// Session ID.
    pub session_id: Option<String>,
    /// Platform key (e.g., "whatsapp", "telegram").
    pub platform: Option<String>,
    /// Whether to skip context files.
    pub skip_context_files: bool,
    /// Working directory for context file discovery.
    pub terminal_cwd: Option<PathBuf>,
    /// Tool-use enforcement mode: "auto", true, false, or custom list.
    pub tool_use_enforcement: ToolUseEnforcement,
    /// Available tool names (for tool-aware guidance injection).
    pub available_tools: Option<HashSet<String>>,
}

/// Tool-use enforcement configuration.
#[derive(Debug, Clone, Default)]
pub enum ToolUseEnforcement {
    /// Auto-detect based on model name.
    #[default]
    Auto,
    /// Always inject.
    Always,
    /// Never inject.
    Never,
    /// Custom model name substrings to match.
    Custom(Vec<String>),
}

/// Builder result.
#[derive(Debug, Clone, Default)]
pub struct PromptBuilderResult {
    /// The assembled system prompt.
    pub system_prompt: String,
    /// Whether SOUL.md was loaded.
    pub soul_loaded: bool,
}

/// Build the system prompt from all layers.
///
/// Layers (in order):
/// 1. Agent identity — SOUL.md when available, else DEFAULT_AGENT_IDENTITY
/// 2. Tool-aware behavioral guidance (memory, session_search, skills)
/// 3. Tool-use enforcement (model-dependent)
/// 4. User/gateway system prompt (if provided)
/// 5. Skills index
/// 6. Context files (AGENTS.md, .cursorrules, etc.)
/// 7. Timestamp/metadata
/// 8. Platform hints
pub fn build_system_prompt(config: &PromptBuilderConfig, system_message: Option<&str>) -> PromptBuilderResult {
    let mut parts: Vec<String> = Vec::new();

    // 1. Agent identity
    let soul_loaded = if !config.skip_context_files {
        if let Some(soul) = load_soul_md() {
            parts.push(soul);
            true
        } else {
            parts.push(DEFAULT_AGENT_IDENTITY.to_string());
            false
        }
    } else {
        parts.push(DEFAULT_AGENT_IDENTITY.to_string());
        false
    };

    // 2. Tool-aware behavioral guidance
    if let Some(tools) = &config.available_tools {
        let mut guidance: Vec<&str> = Vec::new();
        if tools.contains("memory") {
            guidance.push(MEMORY_GUIDANCE);
        }
        if tools.contains("session_search") {
            guidance.push(SESSION_SEARCH_GUIDANCE);
        }
        if tools.contains("skill_manage") {
            guidance.push(SKILLS_GUIDANCE);
        }
        if !guidance.is_empty() {
            parts.push(guidance.join(" "));
        }

        // 3. Tool-use enforcement
        if should_inject_tool_use_enforcement(&config.tool_use_enforcement, config.model.as_deref()) {
            parts.push(TOOL_USE_ENFORCEMENT_GUIDANCE.to_string());

            let model_lower = config.model.as_deref().unwrap_or("").to_lowercase();
            if model_lower.contains("gemini") || model_lower.contains("gemma") {
                parts.push(GOOGLE_MODEL_OPERATIONAL_GUIDANCE.to_string());
            }
            if model_lower.contains("gpt") || model_lower.contains("codex") {
                parts.push(OPENAI_MODEL_EXECUTION_GUIDANCE.to_string());
            }
        }
    }

    // 4. User/gateway system message
    if let Some(msg) = system_message {
        parts.push(msg.to_string());
    }

    // 5. Skills index
    if let Some(tools) = &config.available_tools {
        let has_skills_tools = ["skills_list", "skill_view", "skill_manage"]
            .iter()
            .any(|&name| tools.contains(name));

        if has_skills_tools {
            let skills_prompt =
                build_skills_system_prompt(tools, &HashSet::new());
            if !skills_prompt.is_empty() {
                parts.push(skills_prompt);
            }
        }
    }

    // 6. Context files
    if !config.skip_context_files {
        let cwd = config
            .terminal_cwd
            .as_deref()
            .unwrap_or_else(|| Path::new("."));
        if let Some(context_prompt) = build_context_files_prompt(cwd, soul_loaded) {
            parts.push(context_prompt);
        }
    }

    // 7. Timestamp/metadata
    let timestamp_line = build_timestamp_line(config);
    parts.push(timestamp_line);

    // 8. Platform hints
    if let Some(platform) = &config.platform {
        let platform_lower = platform.to_lowercase();
        for &(key, hint) in platform_hints() {
            if key == platform_lower {
                parts.push(hint.to_string());
                break;
            }
        }
    }

    // 8b. Environment hints (WSL, Termux, etc.)
    if let Some(env_hint) = build_environment_hints() {
        parts.push(env_hint);
    }

    PromptBuilderResult {
        system_prompt: parts.join("\n\n"),
        soul_loaded,
    }
}

/// Check whether to inject tool-use enforcement guidance.
fn should_inject_tool_use_enforcement(enforcement: &ToolUseEnforcement, model: Option<&str>) -> bool {
    match enforcement {
        ToolUseEnforcement::Always => true,
        ToolUseEnforcement::Never => false,
        ToolUseEnforcement::Auto => {
            let model_lower = model.unwrap_or("").to_lowercase();
            TOOL_USE_ENFORCEMENT_MODELS
                .iter()
                .any(|&p| model_lower.contains(p))
        }
        ToolUseEnforcement::Custom(patterns) => {
            let model_lower = model.unwrap_or("").to_lowercase();
            patterns.iter().any(|p| model_lower.contains(&p.to_lowercase()))
        }
    }
}

/// Build the timestamp/metadata line.
fn build_timestamp_line(config: &PromptBuilderConfig) -> String {
    let now = chrono::Local::now();
    let mut line = format!(
        "Conversation started: {}",
        now.format("%A, %B %d, %Y %I:%M %p")
    );

    if let Some(ref session_id) = config.session_id {
        line.push_str(&format!("\nSession ID: {}", session_id));
    }
    if let Some(ref model) = config.model {
        line.push_str(&format!("\nModel: {}", model));
    }
    if let Some(ref provider) = config.provider {
        line.push_str(&format!("\nProvider: {}", provider));
    }

    line
}

/// Truncate content with head/tail split.
fn truncate_content(content: &str, filename: &str, max_chars: usize) -> String {
    if content.len() <= max_chars {
        return content.to_string();
    }
    let head_chars = ((max_chars as f64 * TRUNCATE_HEAD_RATIO) as usize).min(content.len());
    let tail_chars = ((max_chars as f64 * TRUNCATE_TAIL_RATIO) as usize).min(content.len());

    let actual_head = head_chars.min(content.len().saturating_sub(tail_chars));
    let head = &content[..actual_head];
    let tail_start = content.len() - tail_chars;
    let tail = &content[tail_start..];

    let marker = format!(
        "\n\n[...truncated {}: kept {}+{} of {} chars]\n\n",
        filename, actual_head, tail_chars, content.len()
    );

    format!("{}{}{}", head, marker, tail)
}

/// Strip YAML frontmatter.
fn strip_yaml_frontmatter(content: &str) -> &str {
    if let Some(stripped) = content.strip_prefix("---") {
        if let Some(end) = stripped.find("\n---") {
            return content[end + 7..].trim_start();
        }
    }
    content
}

/// Find the git root from a starting path.
fn find_git_root(start: &Path) -> Option<PathBuf> {
    let current = start.canonicalize().ok()?;
    let mut current = current.as_path();

    loop {
        if current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

/// Find the nearest .hermez.md or HERMEZ.md file.
fn find_hermez_md(cwd: &Path) -> Option<PathBuf> {
    let stop_at = find_git_root(cwd);
    let mut current = Some(cwd);

    while let Some(dir) = current {
        for name in HERMEZ_MD_NAMES {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if let Some(ref stop) = stop_at {
            if dir == stop.as_path() {
                break;
            }
        }
        current = dir.parent();
    }

    None
}

/// Load and process a context file with injection scanning and truncation.
fn load_context_file(path: &Path, label: &str, max_chars: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    // Strip YAML frontmatter (e.g., from .hermez.md with model overrides)
    let content = strip_yaml_frontmatter(content);

    let sanitized = sanitize_context_content(content, label);
    let truncated = truncate_content(&sanitized, label, max_chars);

    Some(format!("## {}\n\n{}", label, truncated))
}

/// Build the context files prompt.
///
/// Priority (first found wins — only ONE project context type is loaded):
/// 1. .hermez.md / HERMEZ.md (walk to git root)
/// 2. AGENTS.md / agents.md (cwd only)
/// 3. CLAUDE.md / claude.md (cwd only)
/// 4. .cursorrules / .cursor/rules/*.mdc (cwd only)
///
/// SOUL.md from HERMEZ_HOME is independent and always included when present.
pub fn build_context_files_prompt(cwd: &Path, skip_soul: bool) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();

    // Priority-based project context: first match wins
    let project_context = find_hermez_md(cwd)
        .and_then(|p| load_context_file(&p, ".hermez.md", SOUL_CONTEXT_FILE_MAX_CHARS))
        .or_else(|| {
            // AGENTS.md
            for name in ["AGENTS.md", "agents.md"] {
                let candidate = cwd.join(name);
                if candidate.is_file() {
                    return load_context_file(&candidate, name, SOUL_CONTEXT_FILE_MAX_CHARS);
                }
            }
            None
        })
        .or_else(|| {
            // CLAUDE.md
            for name in ["CLAUDE.md", "claude.md"] {
                let candidate = cwd.join(name);
                if candidate.is_file() {
                    return load_context_file(&candidate, name, SOUL_CONTEXT_FILE_MAX_CHARS);
                }
            }
            None
        })
        .or_else(|| {
            // .cursorrules + .cursor/rules/*.mdc
            let mut cursor_content = String::new();

            let cursorrules = cwd.join(".cursorrules");
            if cursorrules.is_file() {
                if let Some(content) =
                    load_context_file(&cursorrules, ".cursorrules", SOUL_CONTEXT_FILE_MAX_CHARS)
                {
                    cursor_content.push_str(&content);
                    cursor_content.push_str("\n\n");
                }
            }

            let cursor_rules_dir = cwd.join(".cursor").join("rules");
            if cursor_rules_dir.is_dir() {
                if let Ok(entries) = std::fs::read_dir(&cursor_rules_dir) {
                    let mut mdc_files: Vec<_> = entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            e.path()
                                .extension()
                                .and_then(|ext| ext.to_str())
                                == Some("mdc")
                        })
                        .map(|e| e.path())
                        .collect();
                    mdc_files.sort();

                    for mdc_file in mdc_files {
                        if let Some(label) = mdc_file.file_name().and_then(|n| n.to_str()) {
                            let full_label = format!(".cursor/rules/{}", label);
                            if let Some(content) =
                                load_context_file(&mdc_file, &full_label, SOUL_CONTEXT_FILE_MAX_CHARS)
                            {
                                cursor_content.push_str(&content);
                                cursor_content.push_str("\n\n");
                            }
                        }
                    }
                }
            }

            if cursor_content.is_empty() {
                None
            } else {
                Some(truncate_content(
                    &cursor_content,
                    ".cursorrules",
                    SOUL_CONTEXT_FILE_MAX_CHARS,
                ))
            }
        });

    if let Some(ctx) = project_context {
        sections.push(ctx);
    }

    // SOUL.md from HERMEZ_HOME only — skip when already loaded as identity
    if !skip_soul {
        if let Some(soul) = load_soul_md() {
            sections.push(soul);
        }
    }

    if sections.is_empty() {
        return None;
    }

    Some(format!(
        "# Project Context\n\n\
        The following project context files have been loaded and should be followed:\n\n{}",
        sections.join("\n")
    ))
}

/// Determine if model should use 'developer' role instead of 'system'.
pub fn should_use_developer_role(model: &str) -> bool {
    let model_lower = model.to_lowercase();
    model_lower.contains("gpt-5") || model_lower.contains("codex")
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_yaml_frontmatter() {
        let content = "---\nname: test\n---\nBody content";
        assert_eq!(strip_yaml_frontmatter(content), "Body content");
    }

    #[test]
    fn test_strip_yaml_frontmatter_no_fm() {
        let content = "# No frontmatter";
        assert_eq!(strip_yaml_frontmatter(content), content);
    }

    #[test]
    fn test_truncate_short_content() {
        let result = truncate_content("short", "test.md", 100);
        assert_eq!(result, "short");
    }

    #[test]
    fn test_truncate_long_content() {
        let content = "a".repeat(30_000);
        let result = truncate_content(&content, "test.md", SOUL_CONTEXT_FILE_MAX_CHARS);
        assert!(result.len() < 30_000);
        assert!(result.contains("truncated"));
    }

    #[test]
    fn test_should_inject_tool_use_enforcement_auto() {
        assert!(should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Auto,
            Some("gpt-4o")
        ));
        assert!(should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Auto,
            Some("gemini-pro")
        ));
        assert!(!should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Auto,
            Some("claude-opus")
        ));
    }

    #[test]
    fn test_should_inject_tool_use_enforcement_always() {
        assert!(should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Always,
            Some("claude-opus")
        ));
    }

    #[test]
    fn test_should_inject_tool_use_enforcement_never() {
        assert!(!should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Never,
            Some("gpt-4o")
        ));
    }

    #[test]
    fn test_should_inject_tool_use_enforcement_custom() {
        assert!(should_inject_tool_use_enforcement(
            &ToolUseEnforcement::Custom(vec!["mistral".to_string()]),
            Some("mistral-large")
        ));
    }

    #[test]
    fn test_should_use_developer_role() {
        assert!(should_use_developer_role("gpt-5-2026-01-01"));
        assert!(should_use_developer_role("codex-mini"));
        assert!(!should_use_developer_role("gpt-4o"));
        assert!(!should_use_developer_role("claude-sonnet"));
    }

    #[test]
    fn test_build_timestamp_line() {
        let config = PromptBuilderConfig {
            model: Some("test-model".to_string()),
            provider: Some("test-provider".to_string()),
            session_id: Some("test-session".to_string()),
            ..Default::default()
        };
        let line = build_timestamp_line(&config);
        assert!(line.contains("Conversation started:"));
        assert!(line.contains("Session ID: test-session"));
        assert!(line.contains("Model: test-model"));
        assert!(line.contains("Provider: test-provider"));
    }

    #[test]
    fn test_build_system_prompt_basic() {
        let config = PromptBuilderConfig {
            skip_context_files: true,
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);
        assert!(!result.system_prompt.is_empty());
        // Should contain default identity
        assert!(result.system_prompt.contains("Hermez Agent"));
        assert!(!result.soul_loaded);
    }

    #[test]
    fn test_platform_hint_injection() {
        let config = PromptBuilderConfig {
            platform: Some("cli".to_string()),
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);
        assert!(result.system_prompt.contains("CLI AI Agent"));
    }

    #[test]
    fn test_e2e_prompt_with_tools_and_enforcement() {
        let mut tools = HashSet::new();
        tools.insert("web_search".to_string());
        tools.insert("read_file".to_string());
        tools.insert("write_file".to_string());
        tools.insert("terminal".to_string());
        tools.insert("memory".to_string());
        tools.insert("session_search".to_string());
        tools.insert("skill_manage".to_string());

        let config = PromptBuilderConfig {
            model: Some("gpt-5-2026-01-01".to_string()),
            provider: Some("openai".to_string()),
            session_id: Some("test-session-123".to_string()),
            platform: Some("cli".to_string()),
            skip_context_files: true,
            tool_use_enforcement: ToolUseEnforcement::Auto,
            available_tools: Some(tools),
            ..Default::default()
        };

        let result = build_system_prompt(&config, None);
        let prompt = &result.system_prompt;

        // Should contain tool-use enforcement (GPT model)
        assert!(prompt.contains("Tool-use enforcement"));

        // Should contain OpenAI guidance
        assert!(prompt.contains("Execution discipline"));

        // Should contain memory guidance
        assert!(prompt.contains("persistent memory"));

        // Should contain session search guidance
        assert!(prompt.contains("session_search"));

        // Should contain skills guidance
        assert!(prompt.contains("skill_manage"));

        // Should contain platform hint
        assert!(prompt.contains("CLI AI Agent"));

        // Should contain timestamp line
        assert!(prompt.contains("test-session-123"));
    }

    #[test]
    fn test_e2e_prompt_with_soul() {
        let hermez_home = hermez_core::get_hermez_home();
        let soul_path = hermez_home.join("SOUL.md");
        let had_soul = soul_path.exists();
        let original_content = had_soul.then(|| std::fs::read_to_string(&soul_path).ok()).flatten();

        // Temporarily write test SOUL.md
        std::fs::write(&soul_path, "# SOUL\n\nYou are a coding assistant.").unwrap();

        let config = PromptBuilderConfig {
            skip_context_files: false,
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);

        // Restore original file
        if let Some(content) = original_content {
            std::fs::write(&soul_path, content).unwrap();
        } else {
            let _ = std::fs::remove_file(&soul_path);
        }

        assert!(result.system_prompt.contains("coding assistant"));
        assert!(result.soul_loaded);
    }

    #[test]
    fn test_e2e_prompt_without_soul_falls_back() {
        let config = PromptBuilderConfig {
            skip_context_files: true,
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);
        assert!(!result.soul_loaded);
        // Should use default identity
        assert!(result.system_prompt.contains("Hermez Agent"));
    }

    #[test]
    fn test_e2e_prompt_with_user_system_prompt() {
        let config = PromptBuilderConfig::default();
        let result = build_system_prompt(&config, Some("You are a helpful assistant focused on security."));

        assert!(result.system_prompt.contains("security"));
        assert!(result.system_prompt.contains("Hermez Agent"));
        // User prompt should appear after the identity layer
        assert!(result.system_prompt.find("security") > result.system_prompt.find("Hermez Agent"));
    }

    #[test]
    fn test_e2e_prompt_with_gateway_platform() {
        let config = PromptBuilderConfig {
            platform: Some("telegram".to_string()),
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);

        assert!(result.system_prompt.contains("Telegram"));
        assert!(result.system_prompt.contains("do not use markdown"));
    }

    #[test]
    fn test_e2e_prompt_with_custom_tool_enforcement() {
        let mut tools = HashSet::new();
        tools.insert("read_file".to_string());

        let config = PromptBuilderConfig {
            model: Some("mistral-large-2".to_string()),
            tool_use_enforcement: ToolUseEnforcement::Custom(vec!["mistral".to_string()]),
            available_tools: Some(tools),
            ..Default::default()
        };
        let result = build_system_prompt(&config, None);
        assert!(result.system_prompt.contains("Tool-use enforcement"));
    }

    #[test]
    fn test_build_environment_hints_non_wsl() {
        // On CI/non-WSL, this should return None
        let hints = build_environment_hints();
        // Could be Some or None depending on environment, just verify it doesn't crash
        if let Some(h) = hints {
            assert!(h.contains("Environment"));
        }
    }

    #[test]
    fn test_is_wsl_not_windows_on_linux() {
        #[cfg(target_os = "linux")]
        {
            // Just verify it runs without panic
            let _ = is_wsl();
        }
        #[cfg(windows)]
        {
            // On native Windows (not WSL), this should be false
            // But in WSL, env var WSL_DISTRO_NAME may be set
            let _ = is_wsl();
        }
    }
}
