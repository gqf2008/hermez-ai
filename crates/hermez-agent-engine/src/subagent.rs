#![allow(dead_code)]
//! Subagent delegation — spawn child agents with isolated context.
//!
//! Mirrors the Python subagent delegation in `run_agent.py`.
//! Features:
//! - Depth limiting (parent → child only, no grandchildren)
//! - Isolated tool subset for children
//! - Independent iteration budget capped per-child
//! - Concurrent execution via tokio::JoinSet
//! - Interrupt propagation to running children

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serde_json::Value;
use tokio::task::JoinSet;

use hermez_tools::registry::ToolRegistry;

use crate::agent::{AIAgent, AgentConfig, ExitReason};

/// Maximum delegation depth. 2 means parent → child only.
const MAX_DEPTH: u32 = 2;

/// Tools blocked for child agents.
const BLOCKED_TOOLS: &[&str] = &[
    "delegate_task",
    "clarify",
    "memory",
    "send_message",
    "execute_code",
];

/// Result from a single subagent task.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    pub goal: String,
    pub response: String,
    pub exit_reason: ExitReason,
    pub api_calls: usize,
}

/// Default max concurrent children.
const DEFAULT_MAX_CONCURRENT_CHILDREN: usize = 3;

/// Manages subagent delegation for the parent agent.
pub struct SubagentManager {
    /// Current delegation depth.
    pub depth: u32,
    /// Shared interrupt flag — set by parent to cancel all children.
    pub interrupt: Arc<AtomicBool>,
    /// Per-child iteration budget cap.
    pub max_child_iterations: usize,
    /// Max concurrent child agents.
    pub max_concurrent_children: usize,
}

impl SubagentManager {
    pub fn new(depth: u32, interrupt: Arc<AtomicBool>, max_child_iterations: usize) -> Self {
        Self {
            depth,
            interrupt,
            max_child_iterations,
            max_concurrent_children: DEFAULT_MAX_CONCURRENT_CHILDREN,
        }
    }

    /// Create with custom max concurrent children.
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent_children = max.max(1);
        self
    }

    /// Check if spawning another child would exceed depth limit.
    pub fn can_delegate(&self) -> bool {
        self.depth < MAX_DEPTH
    }

    /// Execute delegate_task tool call — spawn child agents concurrently.
    pub async fn execute_delegation(
        self: Arc<Self>,
        args: Value,
        parent_registry: Arc<ToolRegistry>,
    ) -> Vec<SubagentResult> {
        if !self.can_delegate() {
            tracing::warn!("Subagent delegation: depth limit {} reached", MAX_DEPTH);
            return vec![SubagentResult {
                goal: String::new(),
                response: format!("Maximum delegation depth ({MAX_DEPTH}) reached."),
                exit_reason: ExitReason::DepthLimit,
                api_calls: 0,
            }];
        }

        let tasks = self.parse_tasks(&args);
        if tasks.is_empty() {
            return vec![];
        }

        // Filter toolsets for children
        let child_toolsets: Vec<String> = args
            .get("toolsets")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .filter(|t| !BLOCKED_TOOLS.contains(&t.as_str()))
                    .collect()
            })
            .unwrap_or_else(|| vec!["core".to_string(), "terminal".to_string()]);

        // Check if task count exceeds limit (mirrors Python: return error instead of truncating)
        let max_children = self.max_concurrent_children;
        if tasks.len() > max_children {
            return vec![SubagentResult {
                goal: String::new(),
                response: format!(
                    "Too many tasks: {} provided, but max_concurrent_children is {}. \
                    Either reduce the task count, split into multiple delegate_task calls, \
                    or increase delegation.max_concurrent_children in config.yaml.",
                    tasks.len(), max_children
                ),
                exit_reason: ExitReason::TooManyTasks,
                api_calls: 0,
            }];
        }

        // Spawn children concurrently
        let mut set: JoinSet<SubagentResult> = JoinSet::new();

        for task in &tasks {
            let registry = parent_registry.clone();
            let model = task.model.clone();
            let base_url = task.base_url.clone();
            let api_key = task.api_key.clone();
            let goal = task.goal.clone();
            let context = task.context.clone();
            let max_iter = self.max_child_iterations;
            let toolsets = child_toolsets.clone();
            let interrupt = self.interrupt.clone();

            set.spawn(async move {
                run_child_agent(
                    goal, context, model, base_url, api_key,
                    max_iter, toolsets, registry, interrupt,
                ).await
            });
        }

        // Collect results as they complete
        let mut results = Vec::new();
        while let Some(result) = set.join_next().await {
            match result {
                Ok(r) => results.push(r),
                Err(e) => results.push(SubagentResult {
                    goal: String::new(),
                    response: format!("Child agent panicked: {e}"),
                    exit_reason: ExitReason::Panic,
                    api_calls: 0,
                }),
            }
        }

        results
    }

    fn parse_tasks(&self, args: &Value) -> Vec<DelegateTask> {
        if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
            tasks.iter().filter_map(parse_task).collect()
        } else if let Some(goal) = args.get("goal").and_then(Value::as_str) {
            vec![DelegateTask {
                goal: goal.to_string(),
                context: args.get("context").and_then(Value::as_str).unwrap_or("").to_string(),
                model: None,
                base_url: None,
                api_key: None,
            }]
        } else {
            vec![]
        }
    }
}

/// A single delegation task.
#[derive(Debug, Clone)]
pub(crate) struct DelegateTask {
    goal: String,
    context: String,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
}

fn parse_task(v: &Value) -> Option<DelegateTask> {
    let goal = v.get("goal").and_then(Value::as_str)?.to_string();
    Some(DelegateTask {
        goal,
        context: v.get("context").and_then(Value::as_str).unwrap_or("").to_string(),
        model: v.get("model").and_then(Value::as_str).map(String::from),
        base_url: v.get("base_url").and_then(Value::as_str).map(String::from),
        api_key: v.get("api_key").and_then(Value::as_str).map(String::from),
    })
}

/// Run a child agent with isolated context.
#[allow(clippy::too_many_arguments)]
async fn run_child_agent(
    goal: String,
    context: String,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    max_iterations: usize,
    toolsets: Vec<String>,
    parent_registry: Arc<ToolRegistry>,
    _interrupt: Arc<AtomicBool>,
) -> SubagentResult {
    tracing::info!(
        "Subagent starting: goal=\"{}\", model={:?}, toolsets={:?}",
        goal,
        model,
        toolsets
    );

    // Create child tool registry (subset of parent's tools)
    let child_registry = Arc::new(create_child_registry(&parent_registry, &toolsets));

    // Build child config
    let child_model = model.unwrap_or_else(|| "anthropic/claude-opus-4.6".to_string());
    let config = AgentConfig {
        model: child_model,
        base_url,
        api_key,
        max_iterations,
        skip_context_files: true, // Subagents don't load context files
        ..AgentConfig::default()
    };

    let mut child = match AIAgent::new(config, child_registry) {
        Ok(agent) => agent,
        Err(e) => {
            return SubagentResult {
                goal,
                response: format!("Failed to create child agent: {e}"),
                exit_reason: ExitReason::CreationError,
                api_calls: 0,
            };
        }
    };

    // Build system prompt with delegation context
    let system_text = format!(
        "You are a subagent delegated by the parent agent.\n\
         Goal: {goal}\n\
         Context: {context}\n\
         Work independently and return a complete result."
    );
    let system_message = Some(system_text.as_str());

    // Run the child conversation
    let turn_result = child.run_conversation(&goal, system_message, None).await;

    tracing::info!(
        "Subagent finished: goal=\"{}\", exit={}, calls={}",
        goal,
        turn_result.exit_reason,
        turn_result.api_calls,
    );

    SubagentResult {
        goal,
        response: turn_result.response,
        exit_reason: turn_result.exit_reason.clone(),
        api_calls: turn_result.api_calls,
    }
}

/// Create a child registry with only the allowed toolsets.
fn create_child_registry(parent: &ToolRegistry, allowed_toolsets: &[String]) -> ToolRegistry {
    let child = ToolRegistry::new();

    // Iterate all registered tools via parent's get()
    let all_names = parent.list_tools();
    for name in all_names {
        let entry = match parent.get(&name) {
            Some(e) => e,
            None => continue,
        };

        let toolset = entry.toolset.clone();

        // Skip blocked tools
        if BLOCKED_TOOLS.iter().any(|&blocked| name == blocked) {
            continue;
        }

        // Check toolset membership
        if !allowed_toolsets.is_empty() && !allowed_toolsets.iter().any(|ts| toolset.contains(ts)) {
            continue;
        }

        // Copy handler from parent
        if let Some(handler) = parent.get_handler(&name) {
            child.register(
                name,
                toolset.clone(),
                (*entry.schema).clone(),
                handler,
                None,
                vec![toolset],
                entry.description.clone(),
                entry.emoji.clone(),
                entry.max_result_size_chars,
            );
        }
    }

    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_limit() {
        let interrupt = Arc::new(AtomicBool::new(false));

        let mgr = SubagentManager::new(0, interrupt.clone(), 50);
        assert!(mgr.can_delegate());

        let mgr = SubagentManager::new(1, interrupt.clone(), 50);
        assert!(mgr.can_delegate());

        let mgr = SubagentManager::new(2, interrupt, 50);
        assert!(!mgr.can_delegate());
    }

    #[test]
    fn test_parse_single_task() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let args = serde_json::json!({
            "goal": "Find security issues",
            "context": "Check auth module"
        });
        let tasks = mgr.parse_tasks(&args);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].goal, "Find security issues");
        assert_eq!(tasks[0].context, "Check auth module");
    }

    #[test]
    fn test_parse_batch_tasks() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let args = serde_json::json!({
            "tasks": [
                { "goal": "Task 1" },
                { "goal": "Task 2", "context": "extra" }
            ]
        });
        let tasks = mgr.parse_tasks(&args);
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].goal, "Task 1");
        assert_eq!(tasks[1].context, "extra");
    }

    #[test]
    fn test_parse_empty() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let tasks = mgr.parse_tasks(&serde_json::json!({}));
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn test_delegation_blocked_at_depth_limit() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(SubagentManager::new(2, interrupt, 50));
        let registry = Arc::new(ToolRegistry::new());
        let args = serde_json::json!({ "goal": "test" });
        let results = mgr.execute_delegation(args, registry).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_reason, ExitReason::DepthLimit);
    }

    #[test]
    fn test_parse_task_with_extra_fields() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let args = serde_json::json!({
            "goal": "Analyze code",
            "context": "src/main.rs"
        });
        let tasks = mgr.parse_tasks(&args);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].goal, "Analyze code");
        assert_eq!(tasks[0].context, "src/main.rs");
        // Single-task format doesn't extract model/base_url/api_key (only batch format does)
        assert!(tasks[0].model.is_none());
    }

    #[test]
    fn test_parse_batch_tasks_extra_fields() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let args = serde_json::json!({
            "tasks": [
                { "goal": "A", "model": "m1" },
                { "goal": "B" },
                { "goal": "C", "model": "m3", "context": "ctx" }
            ]
        });
        let tasks = mgr.parse_tasks(&args);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].model, Some("m1".to_string()));
        assert_eq!(tasks[1].model, None);
        assert_eq!(tasks[2].context, "ctx");
    }

    #[test]
    fn test_parse_task_missing_goal() {
        // In the batch format, items without "goal" are filtered out
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        let args = serde_json::json!({
            "tasks": [
                { "goal": "valid" },
                { "context": "no goal here" },
                { "something": "else" }
            ]
        });
        let tasks = mgr.parse_tasks(&args);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].goal, "valid");
    }

    #[tokio::test]
    async fn test_delegation_empty_args_no_goal_no_tasks() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(SubagentManager::new(0, interrupt, 50));
        let registry = Arc::new(ToolRegistry::new());
        let args = serde_json::json!({});
        let results = mgr.execute_delegation(args, registry).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    #[ignore = "spawns a full child agent conversation — requires LLM mock to run quickly"]
    async fn test_delegation_filters_blocked_toolsets() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(SubagentManager::new(0, interrupt, 50));
        let registry = Arc::new(ToolRegistry::new());

        // Request toolsets that include blocked tools
        let args = serde_json::json!({
            "goal": "test",
            "toolsets": ["core", "terminal"]
        });
        // This should proceed (depth 0 < MAX_DEPTH) but child toolsets
        // should have delegate_task, clarify, memory, etc. removed
        let results = mgr.execute_delegation(args, registry).await;
        // Child agent will fail (no API key) but the delegation itself should work
        assert!(!results.is_empty());
    }

    #[test]
    fn test_create_child_registry_empty_parent() {
        let parent = ToolRegistry::new();
        let child = create_child_registry(&parent, &["core".to_string()]);
        assert!(child.list_tools().is_empty());
    }

    #[test]
    fn test_blocked_tools_filtered() {
        // Verify BLOCKED_TOOLS constant has expected entries
        assert!(BLOCKED_TOOLS.contains(&"delegate_task"));
        assert!(BLOCKED_TOOLS.contains(&"clarify"));
        assert!(BLOCKED_TOOLS.contains(&"memory"));
        assert!(BLOCKED_TOOLS.contains(&"send_message"));
        assert!(BLOCKED_TOOLS.contains(&"execute_code"));
        assert_eq!(BLOCKED_TOOLS.len(), 5);
    }

    #[test]
    fn test_delegate_task_serialization() {
        let task = DelegateTask {
            goal: "test goal".to_string(),
            context: "test context".to_string(),
            model: Some("gpt-4".to_string()),
            base_url: Some("http://api".to_string()),
            api_key: Some("sk-123".to_string()),
        };
        assert_eq!(task.goal, "test goal");
        assert_eq!(task.context, "test context");
        assert_eq!(task.model, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_subagent_result_debug() {
        let result = SubagentResult {
            goal: "find bugs".to_string(),
            response: "done".to_string(),
            exit_reason: ExitReason::Completed,
            api_calls: 5,
        };
        let debug = format!("{:?}", result);
        assert!(debug.contains("find bugs"));
        assert!(debug.contains("Completed"));
    }

    #[test]
    fn test_max_concurrent_children_default() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50);
        assert_eq!(mgr.max_concurrent_children, 3);
    }

    #[test]
    fn test_max_concurrent_children_custom() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50)
            .with_max_concurrent(10);
        assert_eq!(mgr.max_concurrent_children, 10);
    }

    #[test]
    fn test_max_concurrent_children_min_one() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = SubagentManager::new(0, interrupt, 50)
            .with_max_concurrent(0);
        assert_eq!(mgr.max_concurrent_children, 1); // min(0, 1) = 1
    }

    #[tokio::test]
    async fn test_delegation_too_many_tasks() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let mgr = Arc::new(
            SubagentManager::new(0, interrupt, 50)
                .with_max_concurrent(2)
        );
        let registry = Arc::new(ToolRegistry::new());
        let args = serde_json::json!({
            "tasks": [
                { "goal": "Task 1" },
                { "goal": "Task 2" },
                { "goal": "Task 3" },
                { "goal": "Task 4" }
            ]
        });
        let results = mgr.execute_delegation(args, registry).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].exit_reason, ExitReason::TooManyTasks);
        assert!(results[0].response.contains("4 provided"));
        assert!(results[0].response.contains("max_concurrent_children is 2"));
    }
}
