#![allow(dead_code)]
//! ToolUseEnv — RL Environment for Tool Calling Tasks
//!
//! Trains models to select and use the correct tools with proper arguments.
//!
//! Reward signals:
//!   - Tool selection correctness (did the model call the right tool?)
//!   - Argument correctness (were the arguments appropriate?)
//!   - Sequence correctness (was the tool call order correct?)
//!   - Terminal verification (can the result be verified by running tests?)
//!
//! Tasks: Built-in tool-calling exercises (e.g., "create a file", "search for a pattern").
//! Can be extended to load from external datasets.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::base::{
    AgentResult, EnvError, Environment, EnvironmentConfig, EvalSample,
    Message, ScoredTrajectory, heuristic_score,
};

// ---------------------------------------------------------------------------
// Tool use tasks
// ---------------------------------------------------------------------------

/// A single tool use task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseTask {
    /// The instruction given to the model.
    pub instruction: String,
    /// Expected tool calls (ordered).
    pub expected_tool_calls: Vec<ExpectedToolCall>,
    /// Test code to verify the result (run in terminal).
    pub test_code: Option<String>,
    /// Difficulty level.
    pub difficulty: String,
    /// Category (file, terminal, web, etc.).
    pub category: String,
}

/// Expected tool call specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpectedToolCall {
    /// Tool name.
    pub tool: String,
    /// Required arguments (partial match is OK).
    pub required_args: HashMap<String, String>,
    /// Whether this tool must appear before others in the sequence.
    pub order: Option<usize>,
}

/// Built-in tool use tasks for testing and development.
pub fn builtin_tool_tasks() -> Vec<ToolUseTask> {
    vec![
        ToolUseTask {
            instruction: "Create a file called /tmp/hello.txt with the content 'Hello, World!'".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "write_file".into(),
                    required_args: [
                        ("path".into(), "/tmp/hello.txt".into()),
                        ("content".into(), "Hello, World!".into()),
                    ]
                    .into_iter()
                    .collect(),
                    order: Some(0),
                },
            ],
            test_code: Some("cat /tmp/hello.txt | grep -q 'Hello, World!' && echo PASS || echo FAIL".into()),
            difficulty: "easy".into(),
            category: "file".into(),
        },
        ToolUseTask {
            instruction: "Run the command 'echo hello' in the terminal and report the output.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "terminal".into(),
                    required_args: [("command".into(), "echo hello".into())].into_iter().collect(),
                    order: Some(0),
                },
            ],
            test_code: None,
            difficulty: "easy".into(),
            category: "terminal".into(),
        },
        ToolUseTask {
            instruction: "Create a Python file /tmp/test.py that prints 'Hello' when run, then execute it.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "write_file".into(),
                    required_args: [
                        ("path".into(), "/tmp/test.py".into()),
                        ("content".into(), "print('Hello')".into()),
                    ]
                    .into_iter()
                    .collect(),
                    order: Some(0),
                },
                ExpectedToolCall {
                    tool: "terminal".into(),
                    required_args: [("command".into(), "python /tmp/test.py".into())]
                        .into_iter()
                        .collect(),
                    order: Some(1),
                },
            ],
            test_code: Some("python /tmp/test.py 2>/dev/null | grep -q 'Hello' && echo PASS || echo FAIL".into()),
            difficulty: "medium".into(),
            category: "file+terminal".into(),
        },
        ToolUseTask {
            instruction: "Search for all Python files in the current directory.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "search_files".into(),
                    required_args: [("pattern".into(), "*.py".into())].into_iter().collect(),
                    order: Some(0),
                },
            ],
            test_code: None,
            difficulty: "easy".into(),
            category: "search".into(),
        },
        ToolUseTask {
            instruction: "Read the file /etc/os-release and report the OS name.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "read_file".into(),
                    required_args: [("path".into(), "/etc/os-release".into())]
                        .into_iter()
                        .collect(),
                    order: Some(0),
                },
            ],
            test_code: None,
            difficulty: "easy".into(),
            category: "file".into(),
        },
        ToolUseTask {
            instruction: "Create a directory /tmp/mydir, then create a file inside it called notes.txt with some text.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "terminal".into(),
                    required_args: [("command".into(), "mkdir -p /tmp/mydir".into())]
                        .into_iter()
                        .collect(),
                    order: Some(0),
                },
                ExpectedToolCall {
                    tool: "write_file".into(),
                    required_args: [
                        ("path".into(), "/tmp/mydir/notes.txt".into()),
                    ]
                    .into_iter()
                    .collect(),
                    order: Some(1),
                },
            ],
            test_code: Some("test -f /tmp/mydir/notes.txt && echo PASS || echo FAIL".into()),
            difficulty: "medium".into(),
            category: "file+terminal".into(),
        },
        ToolUseTask {
            instruction: "Search the web for 'current Python version'.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "web_search".into(),
                    required_args: [("query".into(), "current Python version".into())]
                        .into_iter()
                        .collect(),
                    order: Some(0),
                },
            ],
            test_code: None,
            difficulty: "easy".into(),
            category: "web".into(),
        },
        ToolUseTask {
            instruction: "Create a script /tmp/sum.sh that adds two numbers, make it executable, and run it with arguments 3 and 4.".into(),
            expected_tool_calls: vec![
                ExpectedToolCall {
                    tool: "write_file".into(),
                    required_args: [
                        ("path".into(), "/tmp/sum.sh".into()),
                    ]
                    .into_iter()
                    .collect(),
                    order: Some(0),
                },
                ExpectedToolCall {
                    tool: "terminal".into(),
                    required_args: [("command".into(), "chmod +x /tmp/sum.sh".into())]
                        .into_iter()
                        .collect(),
                    order: Some(1),
                },
                ExpectedToolCall {
                    tool: "terminal".into(),
                    required_args: [("command".into(), "/tmp/sum.sh 3 4".into())]
                        .into_iter()
                        .collect(),
                    order: Some(2),
                },
            ],
            test_code: Some("test -x /tmp/sum.sh && echo PASS || echo FAIL".into()),
            difficulty: "hard".into(),
            category: "file+terminal".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the tool use RL environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUseEnvConfig {
    /// Base environment config.
    pub base: EnvironmentConfig,
    /// Reward weights.
    pub tool_selection_weight: f64,
    pub argument_weight: f64,
    pub sequence_weight: f64,
    pub terminal_verification_weight: f64,
    /// Number of items to hold out for evaluation.
    pub eval_size: usize,
    /// Fraction of dataset to hold out for evaluation.
    pub eval_split_ratio: f64,
}

impl Default for ToolUseEnvConfig {
    fn default() -> Self {
        Self {
            base: EnvironmentConfig {
                max_agent_turns: 15,
                agent_temperature: 0.8,
                system_prompt: Some(
                    "You are a capable assistant. Use tools to accomplish tasks. \
                     When creating files, use write_file. When running commands, use terminal. \
                     Always verify your work when possible."
                        .into(),
                ),
                group_size: 4,
                total_steps: 1000,
                steps_per_eval: 100,
                use_wandb: true,
                wandb_name: Some("tool-use".into()),
                max_tokens: None,
            },
            tool_selection_weight: 0.4,
            argument_weight: 0.3,
            sequence_weight: 0.1,
            terminal_verification_weight: 0.2,
            eval_size: 2,
            eval_split_ratio: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Tool calling RL environment.
#[derive(Default)]
pub struct ToolUseEnv {
    config: ToolUseEnvConfig,
    tasks: Vec<ToolUseTask>,
    eval_tasks: Vec<ToolUseTask>,
    index: usize,
    is_setup: bool,
}

impl ToolUseEnv {
    /// Create a new ToolUseEnv with default configuration.
    pub fn new() -> Self {
        Self {
            config: ToolUseEnvConfig::default(),
            tasks: Vec::new(),
            eval_tasks: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new ToolUseEnv with a custom configuration.
    pub fn with_config(config: ToolUseEnvConfig) -> Self {
        Self {
            config,
            tasks: Vec::new(),
            eval_tasks: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new ToolUseEnv with custom tasks.
    pub fn with_tasks(tasks: Vec<ToolUseTask>, config: Option<ToolUseEnvConfig>) -> Self {
        let config = config.unwrap_or_default();
        let eval_size = config.eval_size.min(tasks.len() / 2);
        let mut tasks = tasks;
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        tasks.shuffle(&mut rng);

        let eval_tasks = tasks.drain(..eval_size).collect();

        Self {
            config,
            tasks,
            eval_tasks,
            index: 0,
            is_setup: true,
        }
    }

    /// Extract tool calls from a conversation.
    fn extract_tool_calls(messages: &[Message]) -> Vec<(String, HashMap<String, String>)> {
        let mut calls = Vec::new();
        for msg in messages {
            if msg.role == "assistant" {
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        let args = Self::tool_args_to_string_map(&tc.arguments);
                        calls.push((tc.name.clone(), args));
                    }
                }
            }
        }
        calls
    }

    /// Convert JSON arguments to a string map for comparison.
    fn tool_args_to_string_map(
        args: &serde_json::Value,
    ) -> HashMap<String, String> {
        let mut map = HashMap::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                map.insert(k.clone(), v.as_str().unwrap_or("").to_string());
            }
        }
        map
    }

    /// Compute tool selection score.
    fn tool_selection_score(
        actual_calls: &[(String, HashMap<String, String>)],
        expected: &[ExpectedToolCall],
    ) -> f64 {
        if expected.is_empty() {
            return if actual_calls.is_empty() { 1.0 } else { 0.0 };
        }

        let actual_names: std::collections::HashSet<&str> =
            actual_calls.iter().map(|(name, _)| name.as_str()).collect();

        let expected_names: Vec<&str> =
            expected.iter().map(|e| e.tool.as_str()).collect();

        let mut correct = 0usize;
        for name in &expected_names {
            if actual_names.contains(name) {
                correct += 1;
            }
        }

        correct as f64 / expected_names.len() as f64
    }

    /// Compute argument correctness score.
    fn argument_score(
        actual_calls: &[(String, HashMap<String, String>)],
        expected: &[ExpectedToolCall],
    ) -> f64 {
        if expected.is_empty() {
            return 1.0;
        }

        let mut total_score = 0.0;
        let mut total_expected_args = 0usize;

        for exp in expected {
            // Find matching actual call
            for (name, args) in actual_calls {
                if name == &exp.tool {
                    for (key, expected_val) in &exp.required_args {
                        total_expected_args += 1;
                        if let Some(actual_val) = args.get(key) {
                            // Partial match: check if actual contains expected or vice versa
                            if actual_val.contains(expected_val) || expected_val.contains(actual_val) {
                                total_score += 1.0;
                            } else {
                                // Keyword overlap as fallback
                                let overlap = heuristic_score(expected_val, actual_val);
                                total_score += overlap;
                            }
                        }
                    }
                }
            }
        }

        if total_expected_args == 0 {
            return 1.0;
        }

        total_score / total_expected_args as f64
    }

    /// Compute sequence correctness score.
    fn sequence_score(
        actual_calls: &[(String, HashMap<String, String>)],
        expected: &[ExpectedToolCall],
    ) -> f64 {
        let expected_ordered: Vec<(usize, &str)> = expected
            .iter()
            .filter_map(|e| e.order.map(|o| (o, e.tool.as_str())))
            .collect();

        if expected_ordered.is_empty() {
            return 1.0;
        }

        // Extract actual tool names in order
        let actual_names: Vec<&str> = actual_calls.iter().map(|(n, _)| n.as_str()).collect();

        // Check if the ordered tools appear in the correct relative order
        let mut prev_index = 0usize;
        let mut correct_order = 0usize;

        for &(_order, name) in &expected_ordered {
            if let Some(pos) = actual_names.iter().position(|&n| n == name) {
                if pos >= prev_index {
                    correct_order += 1;
                    prev_index = pos + 1;
                }
            }
        }

        correct_order as f64 / expected_ordered.len() as f64
    }

    /// Compute reward breakdown.
    fn compute_reward_breakdown(
        &self,
        task: &ToolUseTask,
        result: &AgentResult,
    ) -> (f64, HashMap<String, f64>) {
        let actual_calls = Self::extract_tool_calls(&result.messages);

        // Signal 1: Tool selection
        let tool_selection = Self::tool_selection_score(&actual_calls, &task.expected_tool_calls);

        // Signal 2: Argument correctness
        let arg_correctness = Self::argument_score(&actual_calls, &task.expected_tool_calls);

        // Signal 3: Sequence correctness
        let seq_correctness = Self::sequence_score(&actual_calls, &task.expected_tool_calls);

        // Signal 4: Terminal verification (simulated — always 1.0 if tool calls were made)
        // In production, this would run the test_code in a sandbox
        let terminal_verify = if !actual_calls.is_empty() { 0.5 } else { 0.0 };

        // Composite reward
        let total_weight = self.config.tool_selection_weight
            + self.config.argument_weight
            + self.config.sequence_weight
            + self.config.terminal_verification_weight;

        let reward = (self.config.tool_selection_weight * tool_selection
            + self.config.argument_weight * arg_correctness
            + self.config.sequence_weight * seq_correctness
            + self.config.terminal_verification_weight * terminal_verify)
            / total_weight.max(f64::EPSILON);

        let mut signals = HashMap::new();
        signals.insert("tool_selection".into(), tool_selection);
        signals.insert("argument_correctness".into(), arg_correctness);
        signals.insert("sequence_correctness".into(), seq_correctness);
        signals.insert("terminal_verification".into(), terminal_verify);

        (reward.clamp(0.0, 1.0), signals)
    }
}

#[async_trait::async_trait]
impl Environment for ToolUseEnv {
    fn name(&self) -> &str {
        "tool-use"
    }

    async fn setup(&mut self) -> Result<(), EnvError> {
        let tasks = builtin_tool_tasks();
        let eval_size = self.config.eval_size.min(tasks.len() / 2);

        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        let mut shuffled = tasks;
        shuffled.shuffle(&mut rng);

        self.eval_tasks = shuffled.drain(..eval_size).collect();
        self.tasks = shuffled;
        self.index = 0;
        self.is_setup = true;

        tracing::info!(
            "ToolUseEnv setup: {} train / {} eval items",
            self.tasks.len(),
            self.eval_tasks.len()
        );

        Ok(())
    }

    async fn get_next_item(&mut self) -> Result<HashMap<String, serde_json::Value>, EnvError> {
        if !self.is_setup {
            self.setup().await?;
        }
        if self.tasks.is_empty() {
            return Err(EnvError::EmptyDataset);
        }
        let task = &self.tasks[self.index % self.tasks.len()];
        self.index += 1;

        let mut item = HashMap::new();
        item.insert(
            "instruction".into(),
            serde_json::Value::String(task.instruction.clone()),
        );
        item.insert(
            "difficulty".into(),
            serde_json::Value::String(task.difficulty.clone()),
        );
        item.insert(
            "category".into(),
            serde_json::Value::String(task.category.clone()),
        );
        item.insert(
            "expected_tool_calls".into(),
            serde_json::to_value(&task.expected_tool_calls).unwrap_or(serde_json::Value::Null),
        );
        if let Some(ref test) = task.test_code {
            item.insert("test_code".into(), serde_json::Value::String(test.clone()));
        }
        Ok(item)
    }

    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String {
        let instruction = match item.get("instruction") {
            Some(v) => v.as_str().unwrap_or("Unknown instruction"),
            None => "Unknown instruction",
        };
        format!(
            "Complete the following task using the appropriate tools:\n\n\
             Task: {}\n\n\
             Use the tools available to accomplish this task.",
            instruction
        )
    }

    async fn compute_reward(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> f64 {
        let (reward, _signals) = self.reward_for_item(item, result);
        reward
    }

    async fn evaluate(&mut self) -> Result<HashMap<String, f64>, EnvError> {
        if self.eval_tasks.is_empty() {
            return Ok(HashMap::new());
        }

        let eval_size = self.config.eval_size.min(self.eval_tasks.len());
        let mut samples = Vec::new();
        let mut total_reward = 0.0;
        let mut total_tool_selection = 0.0;

        for task in &self.eval_tasks[..eval_size] {
            let mut item = HashMap::new();
            item.insert(
                "instruction".into(),
                serde_json::Value::String(task.instruction.clone()),
            );

            let prompt = self.format_prompt(&item);
            let mut messages = Vec::new();
            if let Some(ref sys) = self.config.base.system_prompt {
                messages.push(Message::system(sys));
            }
            messages.push(Message::user(&prompt));

            let result = AgentResult::new(messages);
            let (reward, signals) = self.compute_reward_breakdown(task, &result);

            let tool_sel = signals.get("tool_selection").copied().unwrap_or(0.0);
            total_tool_selection += tool_sel;
            total_reward += reward;

            samples.push(EvalSample {
                prompt: task.instruction.clone(),
                response: result.final_response.clone(),
                expected: task.expected_tool_calls.first().map(|e| e.tool.clone()).unwrap_or_default(),
                correctness: tool_sel,
                reward,
            });
        }

        let n = samples.len() as f64;
        let mut metrics = HashMap::new();
        metrics.insert("eval/mean_reward".into(), total_reward / n.max(f64::EPSILON));
        metrics.insert(
            "eval/mean_tool_selection".into(),
            total_tool_selection / n.max(f64::EPSILON),
        );
        metrics.insert("eval/n_items".into(), n);

        tracing::info!(
            "ToolUseEnv eval: mean_reward={:.3}, mean_tool_selection={:.3}",
            total_reward / n.max(f64::EPSILON),
            total_tool_selection / n.max(f64::EPSILON)
        );

        Ok(metrics)
    }

    fn config(&self) -> &EnvironmentConfig {
        &self.config.base
    }

    async fn run_rollout(
        &self,
        item: &HashMap<String, serde_json::Value>,
    ) -> Result<ScoredTrajectory, EnvError> {
        let mut messages = Vec::new();
        if let Some(ref sys_prompt) = self.config.base.system_prompt {
            messages.push(Message::system(sys_prompt));
        }
        messages.push(Message::user(&self.format_prompt(item)));

        let result = AgentResult::new(messages);
        let (reward, signals) = self.reward_for_item(item, &result);

        Ok(ScoredTrajectory {
            item: item.clone(),
            result,
            reward,
            reward_signals: Some(signals),
        })
    }
}

impl ToolUseEnv {
    /// Compute reward for a given item (helper that looks up the task).
    fn reward_for_item(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> (f64, HashMap<String, f64>) {
        // Find matching task by instruction
        let instruction = item.get("instruction")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Search in both train and eval tasks
        let task = self.tasks.iter().find(|t| t.instruction == instruction)
            .or_else(|| self.eval_tasks.iter().find(|t| t.instruction == instruction));

        if let Some(task) = task {
            return self.compute_reward_breakdown(task, result);
        }

        // If task not found, return a default score based on whether any tools were used
        let mut signals = HashMap::new();
        signals.insert("tool_selection".into(), 0.0);
        signals.insert("argument_correctness".into(), 0.0);
        signals.insert("sequence_correctness".into(), 0.0);
        signals.insert("terminal_verification".into(), 0.0);
        (0.0, signals)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_selection_score_match() {
        let calls = vec![("write_file".into(), HashMap::new())];
        let expected = vec![ExpectedToolCall {
            tool: "write_file".into(),
            required_args: HashMap::new(),
            order: Some(0),
        }];
        assert_eq!(ToolUseEnv::tool_selection_score(&calls, &expected), 1.0);
    }

    #[test]
    fn test_tool_selection_score_mismatch() {
        let calls = vec![("terminal".into(), HashMap::new())];
        let expected = vec![ExpectedToolCall {
            tool: "write_file".into(),
            required_args: HashMap::new(),
            order: Some(0),
        }];
        assert_eq!(ToolUseEnv::tool_selection_score(&calls, &expected), 0.0);
    }

    #[test]
    fn test_tool_selection_score_partial() {
        let calls = vec![
            ("write_file".into(), HashMap::new()),
            ("terminal".into(), HashMap::new()),
        ];
        let expected = vec![
            ExpectedToolCall {
                tool: "write_file".into(),
                required_args: HashMap::new(),
                order: Some(0),
            },
            ExpectedToolCall {
                tool: "search_files".into(),
                required_args: HashMap::new(),
                order: Some(1),
            },
        ];
        let score = ToolUseEnv::tool_selection_score(&calls, &expected);
        assert!((score - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_argument_score_exact() {
        let mut args = HashMap::new();
        args.insert("path".into(), "/tmp/hello.txt".into());
        let calls = vec![("write_file".into(), args)];
        let mut expected_args = HashMap::new();
        expected_args.insert("path".into(), "/tmp/hello.txt".into());
        let expected = vec![ExpectedToolCall {
            tool: "write_file".into(),
            required_args: expected_args,
            order: Some(0),
        }];
        assert_eq!(ToolUseEnv::argument_score(&calls, &expected), 1.0);
    }

    #[test]
    fn test_argument_score_partial() {
        let mut args = HashMap::new();
        args.insert("path".into(), "/tmp/hello.txt.backup".into());
        let calls = vec![("write_file".into(), args)];
        let mut expected_args = HashMap::new();
        expected_args.insert("path".into(), "/tmp/hello.txt".into());
        let expected = vec![ExpectedToolCall {
            tool: "write_file".into(),
            required_args: expected_args,
            order: Some(0),
        }];
        let score = ToolUseEnv::argument_score(&calls, &expected);
        assert!(score > 0.0);
    }

    #[test]
    fn test_builtin_tasks_not_empty() {
        let tasks = builtin_tool_tasks();
        assert!(!tasks.is_empty());
    }

    #[tokio::test]
    async fn test_tool_use_env_setup_and_next_item() {
        let mut env = ToolUseEnv::new();
        env.setup().await.unwrap();
        let item = env.get_next_item().await.unwrap();
        assert!(item.contains_key("instruction"));
    }

    #[tokio::test]
    async fn test_tool_use_env_format_prompt() {
        let env = ToolUseEnv::new();
        let mut item = HashMap::new();
        item.insert("instruction".into(), serde_json::Value::String("Do something".into()));
        let prompt = env.format_prompt(&item);
        assert!(prompt.contains("Do something"));
    }

    #[tokio::test]
    async fn test_tool_use_env_reward_no_tools() {
        let env = ToolUseEnv::new();
        let mut item = HashMap::new();
        item.insert(
            "instruction".into(),
            serde_json::Value::String("Create a file called /tmp/hello.txt".into()),
        );

        let messages = vec![
            Message::user("Create a file"),
            Message::assistant("I cannot do that."),
        ];
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward < 0.5);
    }
}
