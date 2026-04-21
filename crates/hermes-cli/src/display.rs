//! CLI presentation -- spinner, kawaii faces, tool preview formatting.
//!
//! Pure display functions and classes with no AIAgent dependency.
//! Used by AIAgent._execute_tool_calls for CLI feedback.
//!
//! Mirrors the Python `agent/display.py`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};

// ANSI escape codes
const ANSI_RESET: &str = "\x1b[0m";
#[allow(dead_code)]
const RED: &str = "\x1b[31m";

// Bar characters for context pressure
const BAR_FILLED: &str = "▰";
const BAR_EMPTY: &str = "▱";
const BAR_WIDTH: usize = 20;

// =========================================================================
// Configurable tool preview length (0 = no limit)
// =========================================================================

static TOOL_PREVIEW_MAX_LEN: AtomicIsize = AtomicIsize::new(0);

/// Set the global max length for tool call previews. 0 = no limit.
pub fn set_tool_preview_max_len(n: usize) {
    TOOL_PREVIEW_MAX_LEN.store(n as isize, Ordering::SeqCst);
}

/// Return the configured max preview length (0 = unlimited).
pub fn get_tool_preview_max_len() -> usize {
    let val = TOOL_PREVIEW_MAX_LEN.load(Ordering::SeqCst);
    if val < 0 { 0 } else { val as usize }
}

// =========================================================================
// Tool preview (one-line summary of a tool call's primary argument)
// =========================================================================

fn oneline(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Build a short preview of a tool call's primary argument for display.
///
/// `max_len` controls truncation. `None` (default) defers to the global
/// config; `0` means unlimited.
pub fn build_tool_preview(tool_name: &str, args: &serde_json::Map<String, serde_json::Value>, max_len: Option<usize>) -> Option<String> {
    let max_len = max_len.unwrap_or_else(get_tool_preview_max_len);

    if args.is_empty() {
        return None;
    }

    // Special tool handling
    if tool_name == "process" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let sid = args.get("session_id").and_then(|v| v.as_str()).map(|s| &s[..s.len().min(16)]).unwrap_or("");
        let data = args.get("data").and_then(|v| v.as_str()).map(|s| format!("\"{}\"", &oneline(&s[..s.len().min(20)]))).unwrap_or_default();
        let timeout_val = args.get("timeout");

        let mut parts: Vec<String> = vec![action.to_string()];
        if !sid.is_empty() { parts.push(sid.to_string()); }
        if !data.is_empty() { parts.push(data); }
        if let Some(t) = timeout_val {
            if action == "wait" {
                parts.push(format!("{}s", t));
            }
        }
        let result = parts.join(" ");
        return if result.is_empty() { None } else { Some(result) };
    }

    if tool_name == "todo" {
        if let Some(todos) = args.get("todos") {
            let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);
            let count = todos.as_array().map(|a| a.len()).unwrap_or(0);
            if merge {
                return Some(format!("updating {} task(s)", count));
            } else {
                return Some(format!("planning {} task(s)", count));
            }
        } else {
            return Some("reading task list".to_string());
        }
    }

    if tool_name == "session_search" {
        let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
        let q = oneline(query);
        let preview = if q.len() > 25 {
            format!("recall: \"{}...\"", &q[..25])
        } else {
            format!("recall: \"{}\"", q)
        };
        return Some(preview);
    }

    if tool_name == "memory" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
        match action {
            "add" => {
                let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let c = oneline(content);
                let preview = if c.len() > 25 { format!("{}...", &c[..25]) } else { c };
                return Some(format!("+{}: \"{}\"", target, preview));
            }
            "replace" | "remove" => {
                let old = args.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                let prefix = if action == "replace" { "~" } else { "-" };
                let preview = oneline(old);
                let preview = if preview.len() > 20 { format!("{}...", &preview[..20]) } else { preview };
                return Some(format!("{}{}: \"{}\"", prefix, target, preview));
            }
            _ => return Some(action.to_string()),
        }
    }

    if tool_name == "send_message" {
        let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("?");
        let msg = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
        let msg = oneline(msg);
        let msg = if msg.len() > 20 { format!("{}...", &msg[..17]) } else { msg };
        return Some(format!("to {}: \"{}\"", target, msg));
    }

    if tool_name.starts_with("rl_") {
        let preview = match tool_name {
            "rl_list_environments" => "listing envs",
            "rl_select_environment" => args.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            "rl_get_current_config" => "reading config",
            "rl_edit_config" => {
                let field = args.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let value = args.get("value").and_then(|v| v.as_str()).unwrap_or("");
                return Some(format!("{}={}", field, value));
            }
            "rl_start_training" => "starting",
            "rl_check_status" | "rl_stop_training" | "rl_get_results" => {
                let rid = args.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
                let rid = &rid[..rid.len().min(16)];
                match tool_name {
                    "rl_check_status" => rid,
                    "rl_stop_training" => return Some(format!("stopping {}", rid)),
                    "rl_get_results" => rid,
                    _ => unreachable!(),
                }
            }
            "rl_list_runs" => "listing runs",
            "rl_test_inference" => {
                let steps = args.get("num_steps").and_then(|v| v.as_u64()).unwrap_or(3);
                return Some(format!("{} steps", steps));
            }
            _ => return None,
        };
        return Some(preview.to_string());
    }

    // Generic: try primary args, then fallback keys
    let primary_args_map = [
        ("terminal", "command"), ("web_search", "query"), ("web_extract", "urls"),
        ("read_file", "path"), ("write_file", "path"), ("patch", "path"),
        ("search_files", "pattern"), ("browser_navigate", "url"),
        ("browser_click", "ref"), ("browser_type", "text"),
        ("image_generate", "prompt"), ("text_to_speech", "text"),
        ("vision_analyze", "question"), ("mixture_of_agents", "user_prompt"),
        ("skill_view", "name"), ("skills_list", "category"),
        ("cronjob", "action"), ("execute_code", "code"), ("delegate_task", "goal"),
        ("clarify", "question"), ("skill_manage", "name"),
    ];

    let mut key = primary_args_map.iter().find(|(name, _)| *name == tool_name).map(|(_, k)| *k);

    if key.is_none() {
        for fallback in &["query", "text", "command", "path", "name", "prompt", "code", "goal"] {
            if args.contains_key(*fallback) {
                key = Some(fallback);
                break;
            }
        }
    }

    let key = key?;
    let value = args.get(key)?;

    let preview = match value {
        serde_json::Value::Array(arr) => {
            if arr.is_empty() { return None; }
            arr.first().and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };

    let preview = oneline(&preview);
    if preview.is_empty() {
        return None;
    }

    if max_len > 0 && preview.len() > max_len {
        Some(format!("{}...", &preview[..max_len - 3]))
    } else {
        Some(preview)
    }
}

// =========================================================================
// KawaiiSpinner
// =========================================================================

/// Animated spinner frames.
const SPINNER_DOTS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_BOUNCE: &[&str] = &["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"];
const SPINNER_ARROWS: &[&str] = &["←", "↖", "↑", "↗", "→", "↘", "↓", "↙"];
const SPINNER_SPARKLE: &[&str] = &["⁺", "˚", "*", "✧", "✦", "✧", "*", "˚"];

/// Kawaii face expressions.
const KAWAII_WAITING: &[&str] = &[
    "(｡◕‿◕｡)", "(◕‿◕✿)", "٩(◕‿◕｡)۶", "(✿◠‿◠)", "( ˘▽˘)っ",
    "♪(´ε` )", "(◕ᴗ◕✿)", "ヾ(＾∇＾)", "(≧◡≦)", "(★ω★)",
];

const KAWAII_THINKING: &[&str] = &[
    "(｡•́︿•̀｡)", "(◔_◔)", "(¬‿¬)", "( •_•)>⌐■-■", "(⌐■_■)",
    "(´･_･`)", "◉_◉", "(°ロ°)", "( ˘⌣˘)♡", "ヽ(>∀<☆)☆",
];

/// Spinner type selector.
#[derive(Debug, Clone, Copy, Default)]
pub enum SpinnerType {
    #[default]
    Dots,
    Bounce,
    Arrows,
    Sparkle,
}

impl SpinnerType {
    fn frames(self) -> &'static [&'static str] {
        match self {
            SpinnerType::Dots => SPINNER_DOTS,
            SpinnerType::Bounce => SPINNER_BOUNCE,
            SpinnerType::Arrows => SPINNER_ARROWS,
            SpinnerType::Sparkle => SPINNER_SPARKLE,
        }
    }
}

/// Spinner for CLI feedback during tool execution.
pub struct KawaiiSpinner {
    message: String,
    spinner_type: SpinnerType,
    running: bool,
    frame_idx: usize,
    last_line_len: usize,
    start_time: Option<std::time::Instant>,
    prefix: String,
}

impl KawaiiSpinner {
    /// Create a new spinner.
    pub fn new(message: &str, spinner_type: SpinnerType) -> Self {
        Self {
            message: message.to_string(),
            spinner_type,
            running: false,
            frame_idx: 0,
            last_line_len: 0,
            start_time: None,
            prefix: get_skin_tool_prefix(),
        }
    }

    /// Render one frame of the spinner (callers should call this in their own loop).
    pub fn render_frame(&mut self) -> String {
        let frames = self.spinner_type.frames();
        let frame = frames[self.frame_idx % frames.len()];
        let elapsed = self.start_time.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);

        // Pick kawaii face
        let face_idx = self.frame_idx % KAWAII_WAITING.len();
        let face = if elapsed < 2.0 {
            KAWAII_WAITING[face_idx]
        } else {
            KAWAII_THINKING[self.frame_idx % KAWAII_THINKING.len()]
        };

        let line = format!("  {} {} {} {} ({:.1}s)", face, frame, self.message, self.prefix, elapsed);
        let pad = self.last_line_len.saturating_sub(line.len());
        self.last_line_len = line.len() + pad;
        self.frame_idx += 1;

        format!("\r{}{:width$}", line, "", width = pad)
    }

    /// Start the spinner.
    pub fn start(&mut self) {
        if self.running { return; }
        self.running = true;
        self.start_time = Some(std::time::Instant::now());
    }

    /// Update the spinner message.
    pub fn update_text(&mut self, new_message: &str) {
        self.message = new_message.to_string();
    }

    /// Stop the spinner with a final message.
    pub fn stop(&mut self, final_message: Option<&str>) -> String {
        self.running = false;

        let elapsed = self.start_time.map(|t| format!(" ({:.1}s)", t.elapsed().as_secs_f64())).unwrap_or_default();

        let blanks = " ".repeat(self.last_line_len.max(40) + 5);
        let clear = format!("\r{}\r", blanks);

        if let Some(msg) = final_message {
            format!("{}  {}{}", clear, msg, elapsed)
        } else {
            clear
        }
    }
}

// =========================================================================
// Skin-aware helpers
// =========================================================================

/// Get tool output prefix character from active skin.
pub fn get_skin_tool_prefix() -> String {
    // Default prefix — skin engine integration would go here
    "┊".to_string()
}

// =========================================================================
// Cute tool message (completion line that replaces the spinner)
// =========================================================================

fn truncate(s: &str, n: usize) -> String {
    let max_len = get_tool_preview_max_len();
    if max_len == 0 {
        return s.to_string();
    }
    if s.len() > n {
        format!("{}...", &s[..n.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

fn path_display(p: &str, n: usize) -> String {
    let max_len = get_tool_preview_max_len();
    if max_len == 0 {
        return p.to_string();
    }
    if p.len() > n {
        format!("...{}", &p[p.len().saturating_sub(n - 3)..])
    } else {
        p.to_string()
    }
}

/// Generate a formatted tool completion line for CLI quiet mode.
///
/// Format: `| {emoji} {verb:9} {detail}  {duration}`
pub fn get_cute_tool_message(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
    duration_secs: f64,
    result: Option<&str>,
) -> String {
    let dur = format!("{:.1}s", duration_secs);
    let (is_failure, failure_suffix) = detect_tool_failure(tool_name, result);
    let prefix = get_skin_tool_prefix();

    let wrap = |line: &str| -> String {
        let line = if prefix != "┊" {
            line.replacen("┊", &prefix, 1)
        } else {
            line.to_string()
        };
        if is_failure {
            format!("{}{}", line, failure_suffix)
        } else {
            line
        }
    };

    let result = match tool_name {
        "web_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ 🔍 search    {}  {}", truncate(query, 42), dur))
        }
        "terminal" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ 💻 $         {}  {}", truncate(cmd, 42), dur))
        }
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ 📖 read      {}  {}", path_display(path, 35), dur))
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ ✍️  write     {}  {}", path_display(path, 35), dur))
        }
        "patch" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ 🔧 patch     {}  {}", path_display(path, 35), dur))
        }
        "memory" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
            match action {
                "add" => {
                    let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    wrap(&format!("┊ 🧠 memory    +{}: \"{}\"  {}", target, truncate(content, 30), dur))
                }
                "replace" => {
                    let old = args.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                    wrap(&format!("┊ 🧠 memory    ~{}: \"{}\"  {}", target, truncate(old, 20), dur))
                }
                "remove" => {
                    let old = args.get("old_text").and_then(|v| v.as_str()).unwrap_or("");
                    wrap(&format!("┊ 🧠 memory    -{}: \"{}\"  {}", target, truncate(old, 20), dur))
                }
                _ => wrap(&format!("┊ 🧠 memory    {}  {}", action, dur)),
            }
        }
        "session_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            wrap(&format!("┊ 🔍 recall    \"{}\"  {}", truncate(query, 35), dur))
        }
        "todo" => {
            if let Some(todos) = args.get("todos") {
                let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);
                let count = todos.as_array().map(|a| a.len()).unwrap_or(0);
                if merge {
                    wrap(&format!("┊ 📋 plan      update {} task(s)  {}", count, dur))
                } else {
                    wrap(&format!("┊ 📋 plan      {} task(s)  {}", count, dur))
                }
            } else {
                wrap(&format!("┊ 📋 plan      reading tasks  {}", dur))
            }
        }
        "cronjob" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("?");
            match action {
                "create" => {
                    let label = args.get("name")
                        .or(args.get("skill"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("task");
                    wrap(&format!("┊ ⏰ cron      create {}  {}", truncate(label, 24), dur))
                }
                "list" => wrap(&format!("┊ ⏰ cron      listing  {}", dur)),
                other => {
                    let job_id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
                    wrap(&format!("┊ ⏰ cron      {} {}  {}", other, job_id, dur))
                }
            }
        }
        "delegate_task" => {
            if let Some(tasks) = args.get("tasks") {
                if tasks.is_array() {
                    let count = tasks.as_array().map(|a| a.len()).unwrap_or(0);
                    wrap(&format!("┊ 🔀 delegate  {} parallel tasks  {}", count, dur))
                } else {
                    let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("");
                    wrap(&format!("┊ 🔀 delegate  {}  {}", truncate(goal, 35), dur))
                }
            } else {
                let goal = args.get("goal").and_then(|v| v.as_str()).unwrap_or("");
                wrap(&format!("┊ 🔀 delegate  {}  {}", truncate(goal, 35), dur))
            }
        }
        _ => {
            let preview = build_tool_preview(tool_name, args, None).unwrap_or_default();
            let tool_display = if tool_name.len() > 9 { &tool_name[..9] } else { tool_name };
            wrap(&format!("┊ ⚡ {:9} {}  {}", tool_display, truncate(&preview, 35), dur))
        }
    };

    result
}

/// Inspect a tool result string for signs of failure.
///
/// Returns `(is_failure, suffix)` where *suffix* is an informational tag.
fn detect_tool_failure(tool_name: &str, result: Option<&str>) -> (bool, String) {
    let Some(result) = result else { return (false, String::new()); };

    if tool_name == "terminal" {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(result) {
            if let Some(code) = data.get("exit_code").and_then(|v| v.as_i64()) {
                if code != 0 {
                    return (true, format!(" [exit {}]", code));
                }
            }
        }
        return (false, String::new());
    }

    if tool_name == "memory" {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(result) {
            if data.get("success").and_then(|v| v.as_bool()) == Some(false) {
                let error = data.get("error").and_then(|v| v.as_str()).unwrap_or("");
                if error.contains("exceed the limit") {
                    return (true, " [full]".to_string());
                }
            }
        }
    }

    // Generic heuristic
    let lower = &result[..result.len().min(500)].to_lowercase();
    if lower.contains(r#""error""#) || lower.contains(r#""failed""#) || result.starts_with("Error") {
        return (true, " [error]".to_string());
    }

    (false, String::new())
}

// =========================================================================
// Inline diff rendering
// =========================================================================

const MAX_INLINE_DIFF_FILES: usize = 6;
const MAX_INLINE_DIFF_LINES: usize = 80;

/// ANSI colors for diff display.
fn diff_dim() -> &'static str { "\x1b[38;2;150;150;150m" }
fn diff_file() -> &'static str { "\x1b[38;2;180;160;255m" }
fn diff_hunk() -> &'static str { "\x1b[38;2;120;120;140m" }
fn diff_minus() -> &'static str { "\x1b[38;2;255;255;255;48;2;120;20;20m" }
fn diff_plus() -> &'static str { "\x1b[38;2;255;255;255;48;2;20;90;20m" }

/// Render unified diff lines in Hermes' inline transcript style.
#[allow(unused_assignments)]
fn render_inline_unified_diff(diff: &str) -> Vec<String> {
    let mut rendered = Vec::new();
    let mut from_file: Option<&str> = None;
    let mut to_file: Option<&str> = None;

    for raw_line in diff.lines() {
        if let Some(stripped) = raw_line.strip_prefix("--- ") {
            from_file = Some(stripped.trim());
            continue;
        }
        if let Some(stripped) = raw_line.strip_prefix("+++ ") {
            to_file = Some(stripped.trim());
            if from_file.is_some() || to_file.is_some() {
                rendered.push(format!(
                    "{}{} → {}{}",
                    diff_file(),
                    from_file.unwrap_or("a/?"),
                    to_file.unwrap_or("b/?"),
                    ANSI_RESET
                ));
            }
            continue;
        }
        if raw_line.starts_with("@@") {
            rendered.push(format!("{}{}{}", diff_hunk(), raw_line, ANSI_RESET));
            continue;
        }
        if raw_line.starts_with('-') {
            rendered.push(format!("{}{}{}", diff_minus(), raw_line, ANSI_RESET));
            continue;
        }
        if raw_line.starts_with('+') {
            rendered.push(format!("{}{}{}", diff_plus(), raw_line, ANSI_RESET));
            continue;
        }
        if raw_line.starts_with(' ') {
            rendered.push(format!("{}{}{}", diff_dim(), raw_line, ANSI_RESET));
            continue;
        }
        if !raw_line.is_empty() {
            rendered.push(raw_line.to_string());
        }
    }

    rendered
}

/// Split a unified diff into per-file sections.
fn split_unified_diff_sections(diff: &str) -> Vec<String> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();

    for line in diff.lines() {
        if line.starts_with("--- ") && !current.is_empty() {
            sections.push(current);
            current = vec![line];
            continue;
        }
        current.push(line);
    }
    if !current.is_empty() {
        sections.push(current);
    }

    sections.into_iter().map(|s| s.join("\n")).collect()
}

/// Render diff sections while capping file count and total line count.
fn summarize_rendered_diff(diff: &str) -> Vec<String> {
    let sections = split_unified_diff_sections(diff);
    let mut rendered: Vec<String> = Vec::new();
    let mut omitted_files = 0;
    let mut omitted_lines = 0;

    for (idx, section) in sections.iter().enumerate() {
        if idx >= MAX_INLINE_DIFF_FILES {
            omitted_files += 1;
            omitted_lines += render_inline_unified_diff(section).len();
            continue;
        }

        let section_lines = render_inline_unified_diff(section);
        let remaining_budget = MAX_INLINE_DIFF_LINES.saturating_sub(rendered.len());
        if remaining_budget == 0 {
            omitted_lines += section_lines.len();
            omitted_files += 1;
            continue;
        }

        if section_lines.len() <= remaining_budget {
            rendered.extend(section_lines);
            continue;
        }

        rendered.extend(section_lines.iter().take(remaining_budget).cloned());
        omitted_lines += section_lines.len() - remaining_budget;
        omitted_files += 1 + sections.len().saturating_sub(idx + 1);
        break;
    }

    if omitted_files > 0 || omitted_lines > 0 {
        let mut summary = format!("… omitted {} diff line(s)", omitted_lines);
        if omitted_files > 0 {
            summary.push_str(&format!(" across {} additional file(s)/section(s)", omitted_files));
        }
        rendered.push(format!("{}{}{}", diff_hunk(), summary, ANSI_RESET));
    }

    rendered
}

/// Render an edit diff inline.
pub fn render_edit_diff(
    tool_name: &str,
    result: Option<&str>,
    diff_text: Option<&str>,
) -> Option<String> {
    // Try to get diff from tool result (patch tool returns diff in JSON)
    let diff = if tool_name == "patch" {
        result.and_then(|r| {
            serde_json::from_str::<serde_json::Value>(r)
                .ok()
                .and_then(|d| d.get("diff").and_then(|v| v.as_str()).map(|s| s.to_string()))
        })
    } else {
        diff_text.map(|s| s.to_string())
    };

    let diff = diff?;
    if diff.trim().is_empty() {
        return None;
    }

    let rendered = summarize_rendered_diff(&diff);
    Some(rendered.join("\n"))
}

// =========================================================================
// Context pressure display
// =========================================================================

/// Build a formatted context pressure line for CLI display.
///
/// The bar and percentage show progress toward the compaction threshold,
/// NOT the raw context window. 100% = compaction fires.
pub fn format_context_pressure(
    compaction_progress: f64,
    threshold_tokens: usize,
    threshold_percent: f64,
    compression_enabled: bool,
) -> String {
    let pct_int = (compaction_progress * 100.0).min(100.0) as usize;
    let filled = (compaction_progress * BAR_WIDTH as f64).min(BAR_WIDTH as f64) as usize;
    let bar = BAR_FILLED.repeat(filled) + BAR_EMPTY.repeat(BAR_WIDTH - filled).as_str();

    let threshold_k = if threshold_tokens >= 1000 {
        format!("{}k", threshold_tokens / 1000)
    } else {
        threshold_tokens.to_string()
    };
    let threshold_pct_int = (threshold_percent * 100.0) as usize;

    let hint = if compression_enabled {
        "compaction approaching"
    } else {
        "no auto-compaction"
    };

    format!(
        "  \x1b[1m\x1b[33m⚠ context {} {}% to compaction{}\x1b[0m  \x1b[2m{} threshold ({}%) · {}\x1b[0m",
        bar, pct_int, ANSI_RESET, threshold_k, threshold_pct_int, hint
    )
}

/// Build a plain-text context pressure notification for messaging platforms.
pub fn format_context_pressure_gateway(
    compaction_progress: f64,
    threshold_percent: f64,
    compression_enabled: bool,
) -> String {
    let pct_int = (compaction_progress * 100.0).min(100.0) as usize;
    let filled = (compaction_progress * BAR_WIDTH as f64).min(BAR_WIDTH as f64) as usize;
    let bar = BAR_FILLED.repeat(filled) + BAR_EMPTY.repeat(BAR_WIDTH - filled).as_str();

    let threshold_pct_int = (threshold_percent * 100.0) as usize;

    let hint = if compression_enabled {
        format!("Context compaction approaching (threshold: {}% of window).", threshold_pct_int)
    } else {
        "Auto-compaction is disabled — context may be truncated.".to_string()
    };

    format!("⚠️ Context: {} {}% to compaction\n{}", bar, pct_int, hint)
}

// =========================================================================
// Local edit snapshot
// =========================================================================

/// Pre-tool filesystem snapshot used to render diffs locally after writes.
#[derive(Debug, Clone, Default)]
pub struct LocalEditSnapshot {
    pub paths: Vec<PathBuf>,
    pub before: std::collections::HashMap<String, Option<String>>,
}

/// Capture before-state for local write previews.
pub fn capture_local_edit_snapshot(
    tool_name: &str,
    function_args: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<LocalEditSnapshot> {
    let args = function_args?;
    let paths = resolve_local_edit_paths(tool_name, args);
    if paths.is_empty() {
        return None;
    }

    let mut before = std::collections::HashMap::new();
    for path in &paths {
        let content = std::fs::read_to_string(path).ok();
        before.insert(path.to_string_lossy().to_string(), content);
    }

    Some(LocalEditSnapshot { paths, before })
}

fn resolve_local_edit_paths(
    tool_name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Vec<PathBuf> {
    let path_val = match tool_name {
        "write_file" | "patch" => args.get("path"),
        _ => return Vec::new(),
    };

    if let Some(path) = path_val.and_then(|v| v.as_str()) {
        let p = PathBuf::from(shellexpand::tilde(path).into_owned());
        return vec![if p.is_absolute() { p } else { std::env::current_dir().unwrap_or_default().join(&p) }];
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oneline() {
        assert_eq!(oneline("hello   world"), "hello world");
        assert_eq!(oneline("line1\nline2"), "line1 line2");
    }

    #[test]
    fn test_build_tool_preview_web_search() {
        let args = serde_json::json!({"query": "rust async programming"});
        let args_map = args.as_object().unwrap().clone();
        let result = build_tool_preview("web_search", &args_map, None);
        assert_eq!(result, Some("rust async programming".to_string()));
    }

    #[test]
    fn test_build_tool_preview_empty_args() {
        let args = serde_json::Map::new();
        let result = build_tool_preview("web_search", &args, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_get_cute_tool_message_web_search() {
        let args = serde_json::json!({"query": "hello world"});
        let args_map = args.as_object().unwrap().clone();
        let result = get_cute_tool_message("web_search", &args_map, 1.5, None);
        assert!(result.contains("search"));
        assert!(result.contains("hello world"));
        assert!(result.contains("1.5s"));
    }

    #[test]
    fn test_detect_tool_failure_terminal_nonzero_exit() {
        let result = r#"{"exit_code": 1, "output": "error"}"#;
        let (is_failure, suffix) = detect_tool_failure("terminal", Some(result));
        assert!(is_failure);
        assert_eq!(suffix, " [exit 1]");
    }

    #[test]
    fn test_detect_tool_failure_success() {
        let result = r#"{"exit_code": 0, "output": "ok"}"#;
        let (is_failure, _) = detect_tool_failure("terminal", Some(result));
        assert!(!is_failure);
    }

    #[test]
    fn test_context_pressure_format() {
        let result = format_context_pressure(0.5, 32000, 0.8, true);
        assert!(result.contains("context"));
        assert!(result.contains("50%"));
        assert!(result.contains("compaction"));
    }

    #[test]
    fn test_context_pressure_gateway() {
        let result = format_context_pressure_gateway(0.75, 0.8, true);
        assert!(result.contains("Context:"));
        assert!(result.contains("75%"));
        assert!(!result.contains("\x1b[")); // No ANSI codes
    }

    #[test]
    fn test_preview_max_len_truncation() {
        set_tool_preview_max_len(10);
        assert_eq!(get_tool_preview_max_len(), 10);
        set_tool_preview_max_len(0);
    }

    #[test]
    fn test_render_inline_unified_diff() {
        let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
        let rendered = render_inline_unified_diff(diff);
        assert!(!rendered.is_empty());
        assert!(rendered.iter().any(|l| l.contains("file.txt")));
    }
}
