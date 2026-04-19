#![allow(dead_code)]
//! Delegate tool — spawn subagents with isolated context.
//!
//! Mirrors the Python `tools/delegate_tool.py`.
//! MVP: single goal-based delegation with depth limit and toolset restriction.
//! Skips batch mode, ACP integration, and actual agent spawning.

use serde_json::Value;

use crate::registry::{tool_error, ToolRegistry};

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

/// Delegation state — tracks depth across calls.
/// In a full implementation, this would be part of the agent context.
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

/// Handle delegate_task tool call.
pub fn handle_delegate(args: Value) -> Result<String, hermes_core::HermesError> {
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

    if let Some(task_array) = tasks {
        handle_batch_delegation(task_array, &context)
    } else {
        handle_single_delegation(&args, &context)
    }
}

/// Handle single goal-based delegation.
fn handle_single_delegation(args: &Value, ctx: &DelegationContext) -> Result<String, hermes_core::HermesError> {
    let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
    let context_str = args.get("context").and_then(Value::as_str).unwrap_or("");
    let max_iterations = args
        .get("max_iterations")
        .and_then(Value::as_u64)
        .unwrap_or(10);

    // Determine child toolsets (intersection of parent tools minus blocked)
    let requested_toolsets = args
        .get("toolsets")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| ctx.available_toolsets.clone());

    // Filter out blocked tools
    let child_toolsets: Vec<_> = requested_toolsets
        .iter()
        .filter(|t| !BLOCKED_TOOLS.contains(&t.as_str()))
        .cloned()
        .collect();

    // In MVP: report what would happen without actually spawning
    Ok(serde_json::json!({
        "success": true,
        "action": "delegate",
        "mode": "single",
        "goal": goal,
        "context": context_str,
        "child_toolsets": child_toolsets,
        "max_iterations": max_iterations,
        "depth": ctx.current_depth + 1,
        "note": "Delegation registered. In full mode, a child agent would be spawned with isolated context.",
    })
    .to_string())
}

/// Handle batch delegation (multiple tasks).
fn handle_batch_delegation(
    tasks: &[Value],
    ctx: &DelegationContext,
) -> Result<String, hermes_core::HermesError> {
    if tasks.is_empty() {
        return Ok(tool_error("Batch delegation requires at least one task."));
    }

    if tasks.len() > 3 {
        return Ok(tool_error("Batch mode supports at most 3 concurrent tasks."));
    }

    let task_summaries: Vec<Value> = tasks
        .iter()
        .map(|t| {
            serde_json::json!({
                "goal": t.get("goal").and_then(Value::as_str).unwrap_or(""),
                "toolsets": t
                    .get("toolsets")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect::<Vec<_>>()
                    }),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "success": true,
        "action": "delegate",
        "mode": "batch",
        "num_tasks": tasks.len(),
        "tasks": task_summaries,
        "depth": ctx.current_depth + 1,
        "note": "Batch delegation registered. Tasks would run concurrently in full mode.",
    })
    .to_string())
}

/// Register delegate_task tool.
pub fn register_delegate_tool(registry: &mut ToolRegistry) {
    registry.register(
        "delegate_task".to_string(),
        "delegation".to_string(),
        serde_json::json!({
            "name": "delegate_task",
            "description": "Spawn subagents with isolated context for complex subtasks. Use 'goal' for single task or 'tasks' for batch (max 3).",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string", "description": "Single task goal description." },
                    "context": { "type": "string", "description": "Background context for the subagent." },
                    "toolsets": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Restricted toolsets for child agent."
                    },
                    "tasks": {
                        "type": "array",
                        "description": "Batch mode: list of {goal, context, toolsets} objects (max 3).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "goal": { "type": "string" },
                                "context": { "type": "string" },
                                "toolsets": { "type": "array", "items": { "type": "string" } }
                            }
                        }
                    },
                    "max_iterations": { "type": "integer", "description": "Per-child iteration budget (default 10)." }
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
        assert_eq!(json["max_iterations"], 5);
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
        let tools = json["child_toolsets"].as_array().unwrap();
        for tool in tools {
            let name = tool.as_str().unwrap();
            assert!(
                !BLOCKED_TOOLS.contains(&name),
                "blocked tool '{name}' should be filtered"
            );
        }
    }
}
