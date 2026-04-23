#![allow(dead_code)]
//! Delegate tool — spawn subagents with isolated context.
//!
//! Mirrors the Python `tools/delegate_tool.py`.
//! Features:
//! - Depth limiting (parent → child only, no grandchildren)
//! - Isolated tool subset for children
//! - Actual spawning via a globally registered `DelegateSpawner`
//! - Progress callbacks (CLI spinner tree-view, gateway batch relay)
//! - Credential overrides (provider, base_url, api_key, api_mode, ACP transport)

use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Maximum delegation depth (parent → child only, no grandchildren).
const MAX_DEPTH: u32 = 2;

/// Blocked tools for child agents.
const BLOCKED_TOOLS: &[&str] = &[
    "delegate_task",
    "clarify",
    "memory",
    "send_message",
    "execute_code",
];

/// Default max concurrent children.
const DEFAULT_MAX_CONCURRENT_CHILDREN: usize = 3;

/// Default max iterations per child.
const DEFAULT_MAX_ITERATIONS: u32 = 50;

// ─── DelegateSpawner trait ──────────────────────────────────────────────────

/// Result from a single subagent task.
#[derive(Debug, Clone)]
pub struct SubagentTaskResult {
    pub task_index: usize,
    pub status: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub api_calls: usize,
    pub duration_seconds: f64,
    pub model: Option<String>,
    pub exit_reason: String,
}

/// A single task specification for delegation.
#[derive(Debug, Clone)]
pub struct DelegateTaskSpec {
    pub goal: String,
    pub context: Option<String>,
    pub toolsets: Vec<String>,
    pub model: Option<String>,
    pub max_iterations: u32,
}

/// Credential overrides for a child agent.
#[derive(Debug, Clone, Default)]
pub struct CredentialOverrides {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_mode: Option<String>,
    pub acp_command: Option<String>,
    pub acp_args: Vec<String>,
}

/// Trait for spawning child agents.
///
/// Implemented by `hermez-agent-engine` (or the main binary) to avoid a
/// circular dependency between `hermez-tools` and `hermez-agent-engine`.
pub trait DelegateSpawner: Send + Sync {
    /// Spawn one or more child agents and return their results.
    fn spawn(
        &self,
        tasks: Vec<DelegateTaskSpec>,
        credentials: CredentialOverrides,
        progress_cb: Option<Box<dyn ChildProgressCallback>>,
    ) -> Result<Vec<SubagentTaskResult>, String>;
}

/// Global spawner registry — set at startup by the engine layer.
static GLOBAL_SPAWNER: Mutex<Option<Arc<dyn DelegateSpawner>>> = Mutex::new(None);

/// Register the global delegate spawner.
///
/// Called once at startup (e.g., from `hermez-agent-engine` or the main binary).
pub fn set_delegate_spawner(spawner: Arc<dyn DelegateSpawner>) {
    let mut guard = GLOBAL_SPAWNER.lock().unwrap();
    *guard = Some(spawner);
}

/// Clear the global delegate spawner.
pub fn clear_delegate_spawner() {
    let mut guard = GLOBAL_SPAWNER.lock().unwrap();
    *guard = None;
}

fn get_spawner() -> Option<Arc<dyn DelegateSpawner>> {
    GLOBAL_SPAWNER.lock().unwrap().clone()
}

// ─── Progress callbacks ─────────────────────────────────────────────────────

/// Trait for child-agent progress callbacks.
///
/// Two display paths:
/// - CLI: prints tree-view lines above the parent's delegation spinner
/// - Gateway: batches tool names and relays to parent's progress callback
pub trait ChildProgressCallback: Send + Sync {
    /// Emit a progress event.
    fn emit(&mut self, event: ProgressEvent);
    /// Flush any batched state (called on completion).
    fn flush(&mut self);
}

/// Progress event types emitted by child agents.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    SubagentStart { goal: String },
    SubagentComplete { status: String, summary: String },
    SubagentThinking { text: String },
    ToolStarted { tool_name: String, preview: String },
    ToolCompleted { tool_name: String },
    BatchRelay { summary: String },
}

/// Build a child progress callback for CLI spinner tree-view display.
///
/// Mirrors Python `_build_child_progress_callback`.
pub fn build_cli_progress_callback(
    task_index: usize,
    goal: &str,
    task_count: usize,
) -> Option<Box<dyn ChildProgressCallback>> {
    // In a real implementation this would hold a reference to the spinner.
    // Here we provide a logger-based fallback that prints tree lines.
    let prefix = if task_count > 1 {
        format!("[{}] ", task_index + 1)
    } else {
        String::new()
    };
    let goal = goal.to_string();
    Some(Box::new(CliProgressCallback {
        prefix,
        goal,
        batch: Vec::new(),
        batch_size: 5,
    }))
}

/// CLI tree-view progress callback.
struct CliProgressCallback {
    prefix: String,
    goal: String,
    batch: Vec<String>,
    batch_size: usize,
}

impl ChildProgressCallback for CliProgressCallback {
    fn emit(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::SubagentStart { goal } => {
                let short = if goal.len() > 55 { &goal[..55] } else { &goal };
                tracing::info!(" {}├─ 🔀 {}", self.prefix, short);
            }
            ProgressEvent::SubagentThinking { text } => {
                let short = if text.len() > 55 { &text[..55] } else { &text };
                tracing::info!(" {}├─ 💭 \"{}\"", self.prefix, short);
            }
            ProgressEvent::ToolStarted { tool_name, preview } => {
                let short = if preview.len() > 35 { &preview[..35] } else { &preview };
                let emoji = tool_emoji(&tool_name);
                if short.is_empty() {
                    tracing::info!(" {}├─ {} {}", self.prefix, emoji, tool_name);
                } else {
                    tracing::info!(" {}├─ {} {}  \"{}\"", self.prefix, emoji, tool_name, short);
                }
                self.batch.push(tool_name);
                if self.batch.len() >= self.batch_size {
                    let summary = self.batch.join(", ");
                    tracing::info!(" {}🔀 {}", self.prefix, summary);
                    self.batch.clear();
                }
            }
            ProgressEvent::ToolCompleted { .. } => {}
            ProgressEvent::BatchRelay { summary } => {
                tracing::info!(" {}🔀 {}", self.prefix, summary);
            }
            ProgressEvent::SubagentComplete { status, summary } => {
                let icon = if status == "completed" { "✓" } else { "✗" };
                tracing::info!(" {}{} {} ({})", self.prefix, icon, self.goal, summary);
            }
        }
    }

    fn flush(&mut self) {
        if !self.batch.is_empty() {
            let summary = self.batch.join(", ");
            tracing::info!(" {}🔀 {}", self.prefix, summary);
            self.batch.clear();
        }
    }
}

/// Build a gateway batch-relay progress callback.
pub fn build_gateway_progress_callback(
    _task_index: usize,
    _task_count: usize,
) -> Option<Box<dyn ChildProgressCallback>> {
    // Placeholder: in a full implementation this would hold a reference to
    // the parent's gateway callback and batch-flush tool names.
    None
}

fn tool_emoji(name: &str) -> &'static str {
    match name {
        "web_search" => "🔍",
        "web_extract" => "📄",
        "web_crawl" => "🕷️",
        "read_file" => "📖",
        "write_file" => "✍️",
        "terminal" | "execute_command" => "💻",
        "delegate_task" => "🔀",
        "memory" => "🧠",
        _ => "🔧",
    }
}

// ─── Delegation context ─────────────────────────────────────────────────────

/// Delegation state — tracks depth across calls.
#[derive(Debug, Clone)]
pub struct DelegationContext {
    pub current_depth: u32,
    pub parent_agent: Option<String>,
    pub available_toolsets: Vec<String>,
}

impl Default for DelegationContext {
    fn default() -> Self {
        Self {
            current_depth: 0,
            parent_agent: None,
            available_toolsets: vec!["core".to_string(), "terminal".to_string()],
        }
    }
}

// ─── Credential resolution ──────────────────────────────────────────────────

/// Parse credential overrides from tool arguments.
fn parse_credential_overrides(args: &Value) -> CredentialOverrides {
    CredentialOverrides {
        provider: args.get("override_provider").and_then(Value::as_str).map(String::from),
        base_url: args.get("override_base_url").and_then(Value::as_str).map(String::from),
        api_key: args.get("override_api_key").and_then(Value::as_str).map(String::from),
        api_mode: args.get("override_api_mode").and_then(Value::as_str).map(String::from),
        acp_command: args.get("acp_command").and_then(Value::as_str).map(String::from),
        acp_args: args
            .get("acp_args")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default(),
    }
}

// ─── Handler ────────────────────────────────────────────────────────────────

/// Handle delegate_task tool call.
pub fn handle_delegate(args: Value) -> Result<String, hermez_core::HermezError> {
    let goal = args.get("goal").and_then(Value::as_str);
    let tasks = args.get("tasks").and_then(Value::as_array);

    if goal.is_none() && tasks.is_none() {
        return Ok(tool_error(
            "delegate_task requires either 'goal' or 'tasks' parameter.",
        ));
    }

    let context = DelegationContext::default();

    // Check depth limit
    if context.current_depth >= MAX_DEPTH {
        return Ok(tool_error(format!(
            "Maximum delegation depth ({MAX_DEPTH}) reached. Cannot spawn subagents from subagents."
        )));
    }

    // Max concurrent children
    let max_children = std::env::var("DELEGATION_MAX_CONCURRENT_CHILDREN")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENT_CHILDREN)
        .max(1);

    // Max iterations
    let max_iterations = args
        .get("max_iterations")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_ITERATIONS as u64) as u32;

    // Credential overrides
    let credentials = parse_credential_overrides(&args);

    // Parse tasks
    let task_specs = if let Some(task_array) = tasks {
        if task_array.is_empty() {
            return Ok(tool_error("Batch delegation requires at least one task."));
        }
        if task_array.len() > max_children {
            return Ok(tool_error(format!(
                "Too many tasks: {} provided, but max_concurrent_children is {}. \
                Either reduce the task count, split into multiple delegate_task calls, \
                or increase delegation.max_concurrent_children in config.yaml.",
                task_array.len(), max_children
            )));
        }
        task_array
            .iter()
            .filter_map(|t| {
                let goal = t.get("goal").and_then(Value::as_str)?;
                Some(DelegateTaskSpec {
                    goal: goal.to_string(),
                    context: t.get("context").and_then(Value::as_str).map(String::from),
                    toolsets: t
                        .get("toolsets")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    model: t.get("model").and_then(Value::as_str).map(String::from),
                    max_iterations,
                })
            })
            .collect::<Vec<_>>()
    } else {
        vec![DelegateTaskSpec {
            goal: goal.unwrap_or("").to_string(),
            context: args.get("context").and_then(Value::as_str).map(String::from),
            toolsets: args
                .get("toolsets")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| context.available_toolsets.clone()),
            model: args.get("model").and_then(Value::as_str).map(String::from),
            max_iterations,
        }]
    };

    if task_specs.is_empty() {
        return Ok(tool_error("No valid tasks provided."));
    }

    // Filter blocked tools from each task's toolsets
    let task_specs: Vec<_> = task_specs
        .into_iter()
        .map(|mut spec| {
            spec.toolsets.retain(|t| !BLOCKED_TOOLS.contains(&t.as_str()));
            spec
        })
        .collect();

    // If a spawner is registered, use it for actual spawning
    if let Some(spawner) = get_spawner() {
        let task_count = task_specs.len();
        let progress_cb: Option<Box<dyn ChildProgressCallback>> = if task_count > 1 {
            // For batch mode, create a composite callback or use the first task's
            build_cli_progress_callback(0, &task_specs[0].goal, task_count)
        } else {
            build_cli_progress_callback(0, &task_specs[0].goal, 1)
        };

        match spawner.spawn(task_specs, credentials, progress_cb) {
            Ok(results) => {
                let json_results: Vec<Value> = results
                    .into_iter()
                    .map(|r| {
                        let mut obj = serde_json::json!({
                            "task_index": r.task_index,
                            "status": r.status,
                            "summary": r.summary,
                            "api_calls": r.api_calls,
                            "duration_seconds": r.duration_seconds,
                            "exit_reason": r.exit_reason,
                        });
                        if let Some(err) = r.error {
                            obj["error"] = Value::String(err);
                        }
                        if let Some(m) = r.model {
                            obj["model"] = Value::String(m);
                        }
                        obj
                    })
                    .collect();
                return Ok(serde_json::json!({
                    "success": true,
                    "results": json_results,
                })
                .to_string());
            }
            Err(e) => {
                return Ok(tool_error(format!("Delegation spawning failed: {e}")));
            }
        }
    }

    // Fallback: return structured info when no spawner is registered
    let n_tasks = task_specs.len();
    let task_summaries: Vec<Value> = task_specs
        .iter()
        .map(|t| {
            serde_json::json!({
                "goal": t.goal,
                "toolsets": t.toolsets,
                "max_iterations": t.max_iterations,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "action": "delegate",
        "mode": if n_tasks == 1 { "single" } else { "batch" },
        "num_tasks": n_tasks,
        "tasks": task_summaries,
        "depth": context.current_depth + 1,
        "note": "No delegate spawner is registered. In full mode, a child agent would be spawned with isolated context."
    })
    .to_string())
}

// ─── Registration ───────────────────────────────────────────────────────────

/// Register delegate_task tool.
pub fn register_delegate_tool(registry: &mut ToolRegistry) {
    registry.register(
        "delegate_task".to_string(),
        "delegation".to_string(),
        serde_json::json!({
            "name": "delegate_task",
            "description": "Spawn one or more subagents to work on tasks in isolated contexts. Each subagent gets its own conversation, terminal session, and toolset. Only the final summary is returned -- intermediate tool results never enter your context window.\n\nTWO MODES (one of 'goal' or 'tasks' is required):\n1. Single task: provide 'goal' (+ optional context, toolsets)\n2. Batch (parallel): provide 'tasks' array with up to 3 items. All run concurrently and results are returned together.\n\nWHEN TO USE delegate_task:\n- Reasoning-heavy subtasks (debugging, code review, research synthesis)\n- Tasks that would flood your context with intermediate data\n- Parallel independent workstreams (research A and B simultaneously)\n\nWHEN NOT TO USE (use these instead):\n- Mechanical multi-step work with no reasoning needed -> use execute_code\n- Single tool call -> just call the tool directly\n- Tasks needing user interaction -> subagents cannot use clarify\n\nIMPORTANT:\n- Subagents have NO memory of your conversation. Pass all relevant info (file paths, error messages, constraints) via the 'context' field.\n- Subagents CANNOT call: delegate_task, clarify, memory, send_message, execute_code.\n- Each subagent gets its own terminal session (separate working directory and state).\n- Results are always returned as an array, one entry per task.",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "What the subagent should accomplish. Be specific and self-contained -- the subagent knows nothing about your conversation history."
                    },
                    "context": {
                        "type": "string",
                        "description": "Background information the subagent needs: file paths, error messages, project structure, constraints. The more specific you are, the better the subagent performs."
                    },
                    "toolsets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Toolsets to enable for this subagent. Default: inherits your enabled toolsets. Common patterns: ['terminal', 'file'] for code work, ['web'] for research, ['browser'] for web interaction, ['terminal', 'file', 'web'] for full-stack tasks."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Batch mode: tasks to run in parallel (limit configurable via delegation.max_concurrent_children, default 3). Each gets its own subagent with isolated context and terminal session. When provided, top-level goal/context/toolsets are ignored.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "goal": { "type": "string", "description": "Task goal" },
                                "context": { "type": "string", "description": "Task-specific context" },
                                "toolsets": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Toolsets for this specific task."
                                },
                                "acp_command": {
                                    "type": "string",
                                    "description": "Per-task ACP command override (e.g. 'claude'). Overrides the top-level acp_command for this task only."
                                },
                                "acp_args": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Per-task ACP args override."
                                }
                            },
                            "required": ["goal"]
                        }
                    },
                    "max_iterations": {
                        "type": "integer",
                        "description": "Max tool-calling turns per subagent (default: 50). Only set lower for simple tasks."
                    },
                    "override_provider": {
                        "type": "string",
                        "description": "Override LLM provider for child agents (e.g. 'openrouter', 'nous'). Enables routing subagents to a different provider than the parent."
                    },
                    "override_base_url": {
                        "type": "string",
                        "description": "Override API base URL for child agents."
                    },
                    "override_api_key": {
                        "type": "string",
                        "description": "Override API key for child agents."
                    },
                    "override_api_mode": {
                        "type": "string",
                        "description": "Override API mode for child agents (e.g. 'chat_completions', 'anthropic_messages')."
                    },
                    "acp_command": {
                        "type": "string",
                        "description": "Override ACP command for child agents (e.g. 'claude', 'copilot'). When set, children use ACP subprocess transport instead of inheriting the parent's transport."
                    },
                    "acp_args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Arguments for the ACP command (default: ['--acp', '--stdio']). Only used when acp_command is set."
                    }
                }
            }
        }),
        std::sync::Arc::new(handle_delegate),
        None,
        vec!["delegation".to_string()],
        "Spawn subagents for parallel task execution".to_string(),
        "🔀".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_missing_goal_and_tasks() {
        let result = handle_delegate(serde_json::json!({}));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_single_delegation() {
        let result = handle_delegate(serde_json::json!({
            "goal": "Analyze the codebase for security issues",
            "context": "Focus on authentication and input validation",
            "max_iterations": 5
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["mode"], "single");
        assert_eq!(json["num_tasks"], 1);
    }

    #[test]
    fn test_batch_delegation() {
        let result = handle_delegate(serde_json::json!({
            "tasks": [
                { "goal": "Task 1" },
                { "goal": "Task 2", "toolsets": ["core"] }
            ]
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["mode"], "batch");
        assert_eq!(json["num_tasks"], 2);
    }

    #[test]
    fn test_batch_too_many_tasks() {
        let result = handle_delegate(serde_json::json!({
            "tasks": [
                { "goal": "Task 1" },
                { "goal": "Task 2" },
                { "goal": "Task 3" },
                { "goal": "Task 4" }
            ]
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_batch_empty_tasks() {
        let result = handle_delegate(serde_json::json!({
            "tasks": []
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert!(json.get("error").is_some());
    }

    #[test]
    fn test_blocked_tools_filtered() {
        let result = handle_delegate(serde_json::json!({
            "goal": "Do something",
            "toolsets": ["core", "terminal"]
        }));
        let json: Value = serde_json::from_str(&result.unwrap()).unwrap();
        let tasks = json["tasks"].as_array().unwrap();
        let tools = tasks[0]["toolsets"].as_array().unwrap();
        for tool in tools {
            let name = tool.as_str().unwrap();
            assert!(
                !BLOCKED_TOOLS.contains(&name),
                "blocked tool '{name}' should be filtered"
            );
        }
    }

    #[test]
    fn test_parse_credential_overrides() {
        let args = serde_json::json!({
            "override_provider": "openrouter",
            "override_base_url": "https://api.openrouter.ai",
            "override_api_key": "sk-test",
            "override_api_mode": "chat_completions",
            "acp_command": "claude",
            "acp_args": ["--acp", "--stdio"]
        });
        let creds = parse_credential_overrides(&args);
        assert_eq!(creds.provider, Some("openrouter".to_string()));
        assert_eq!(creds.base_url, Some("https://api.openrouter.ai".to_string()));
        assert_eq!(creds.api_key, Some("sk-test".to_string()));
        assert_eq!(creds.api_mode, Some("chat_completions".to_string()));
        assert_eq!(creds.acp_command, Some("claude".to_string()));
        assert_eq!(creds.acp_args, vec!["--acp", "--stdio"]);
    }

    #[test]
    fn test_progress_callback_events() {
        let mut cb = CliProgressCallback {
            prefix: "[1] ".to_string(),
            goal: "Test goal".to_string(),
            batch: Vec::new(),
            batch_size: 2,
        };
        cb.emit(ProgressEvent::ToolStarted { tool_name: "web_search".to_string(), preview: "query".to_string() });
        cb.emit(ProgressEvent::ToolStarted { tool_name: "web_extract".to_string(), preview: "url".to_string() });
        // Batch should flush after 2 items
        assert!(cb.batch.is_empty());
    }

    #[test]
    fn test_progress_callback_flush() {
        let mut cb = CliProgressCallback {
            prefix: "".to_string(),
            goal: "Test".to_string(),
            batch: vec!["tool_a".to_string(), "tool_b".to_string()],
            batch_size: 5,
        };
        cb.flush();
        assert!(cb.batch.is_empty());
    }
}
