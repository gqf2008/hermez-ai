#![allow(dead_code)]
//! Base types, traits, and shared infrastructure for RL environments.
//!
//! Mirrors the Python `environments/hermez_base_env.py` and `environments/agent_loop.py`:
//! - `Environment` trait ≈ `HermezAgentBaseEnv`
//! - `AgentResult` ≈ Python `AgentResult` dataclass
//! - `ToolError` ≈ Python `ToolError` dataclass
//! - `AgentLoopConfig` ≈ relevant subset of `HermezAgentEnvConfig`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during environment operation.
#[derive(Debug, thiserror::Error)]
pub enum EnvError {
    #[error("dataset is empty")]
    EmptyDataset,

    #[error("setup failed: {0}")]
    SetupFailed(String),

    #[error("reward computation failed: {0}")]
    RewardFailed(String),

    #[error("agent loop failed: {0}")]
    AgentLoopFailed(String),

    #[error("evaluation failed: {0}")]
    EvalFailed(String),

    #[error("item retrieval failed: {0}")]
    ItemRetrieval(String),

    #[error("generic error: {0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// Message types (simplified OpenAI message format)
// ---------------------------------------------------------------------------

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,          // "system", "user", "assistant", "tool"
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

impl Message {
    pub fn system(content: &str) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn user(content: &str) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn assistant(content: &str) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
        }
    }

    pub fn assistant_with_tools(
        content: &str,
        tool_calls: Vec<ToolCall>,
        reasoning: Option<String>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
            reasoning_content: reasoning,
        }
    }

    pub fn tool_result(tool_call_id: &str, content: &str) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning_content: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tool call types
// ---------------------------------------------------------------------------

/// A structured tool call from the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

impl fmt::Display for ToolCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, self.arguments)
    }
}

// ---------------------------------------------------------------------------
// AgentResult — result of running the agent loop
// ---------------------------------------------------------------------------

/// Record of a tool execution error during the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolError {
    /// Which turn the error occurred on (1-indexed).
    pub turn: usize,
    /// Which tool was called.
    pub tool_name: String,
    /// The arguments passed (truncated).
    pub arguments: String,
    /// The error message.
    pub error: String,
    /// The raw result returned to the model.
    pub tool_result: String,
}

impl ToolError {
    pub fn new(turn: usize, tool_name: String, arguments: String, error: String, tool_result: String) -> Self {
        Self {
            turn,
            tool_name,
            arguments,
            error,
            tool_result,
        }
    }
}

/// Result of running the agent loop for a single rollout.
///
/// Mirrors the Python `AgentResult` dataclass from `environments/agent_loop.py`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    /// Full conversation history in OpenAI message format.
    pub messages: Vec<Message>,
    /// How many LLM calls were made.
    pub turns_used: usize,
    /// True if model stopped calling tools naturally (vs hitting max_turns).
    pub finished_naturally: bool,
    /// Extracted reasoning content per turn.
    pub reasoning_per_turn: Vec<Option<String>>,
    /// Tool errors encountered during the loop.
    pub tool_errors: Vec<ToolError>,
    /// Names of tools that were called during the rollout.
    pub tools_used: Vec<String>,
    /// Total number of tool calls made.
    pub total_tool_calls: usize,
    /// The final assistant response text (last assistant message content).
    pub final_response: String,
}

impl AgentResult {
    pub fn new(messages: Vec<Message>) -> Self {
        let final_response = Self::extract_final_response(&messages);
        Self {
            messages,
            turns_used: 0,
            finished_naturally: false,
            reasoning_per_turn: Vec::new(),
            tool_errors: Vec::new(),
            tools_used: Vec::new(),
            total_tool_calls: 0,
            final_response,
        }
    }

    /// Extract the final assistant response text from the conversation.
    pub fn extract_final_response(messages: &[Message]) -> String {
        messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant" && !m.content.is_empty())
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }

    /// Collect unique tool names used in the conversation.
    pub fn collect_tools_used(messages: &[Message]) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut tools = Vec::new();
        for msg in messages {
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    if seen.insert(tc.name.clone()) {
                        tools.push(tc.name.clone());
                    }
                }
            }
        }
        tools
    }

    /// Count total tool calls across all assistant turns.
    pub fn count_tool_calls(messages: &[Message]) -> usize {
        messages
            .iter()
            .filter_map(|m| m.tool_calls.as_ref())
            .map(|tcs| tcs.len())
            .sum()
    }
}

// ---------------------------------------------------------------------------
// AgentRunner — bridge to real agent loop
// ---------------------------------------------------------------------------

/// Trait for plugging a real agent into an environment's evaluate loop.
///
/// Implementations (e.g. in `hermez-cli` or `src/main.rs`) wrap
/// `AIAgent::run_conversation()` and convert the `TurnResult` into
/// the `AgentResult` expected by the environment.
#[async_trait::async_trait]
pub trait AgentRunner: Send + Sync {
    /// Run the agent loop for a single task.
    ///
    /// `messages` is the initial conversation history (system + user prompt).
    /// The runner should return the full conversation after the agent loop
    /// completes, along with metadata about the run.
    async fn run(&self, messages: Vec<Message>) -> AgentResult;
}

// ---------------------------------------------------------------------------
// ScoredTrajectory — output of a rollout with reward
// ---------------------------------------------------------------------------

/// A scored trajectory, analogous to Atropos's `ScoredDataItem`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoredTrajectory {
    /// The dataset item this trajectory was generated from.
    pub item: HashMap<String, serde_json::Value>,
    /// The agent's rollout result.
    pub result: AgentResult,
    /// The scalar reward for this trajectory.
    pub reward: f64,
    /// Breakdown of reward signals (optional, for logging).
    pub reward_signals: Option<HashMap<String, f64>>,
}

// ---------------------------------------------------------------------------
// RewardSignal — individual reward components
// ---------------------------------------------------------------------------

/// Named reward signal used in composite reward computation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardSignal {
    pub name: String,
    pub value: f64,
    pub weight: f64,
}

impl RewardSignal {
    pub fn new(name: &str, value: f64, weight: f64) -> Self {
        Self {
            name: name.into(),
            value,
            weight,
        }
    }

    pub fn weighted_value(&self) -> f64 {
        self.value * self.weight
    }
}

/// Compute a composite reward from weighted signals, clamped to [0.0, 1.0].
pub fn composite_reward(signals: &[RewardSignal]) -> f64 {
    let total_weight: f64 = signals.iter().map(|s| s.weight).sum();
    if total_weight == 0.0 {
        return 0.0;
    }
    let raw: f64 = signals.iter().map(|s| s.weighted_value()).sum();
    // Normalize by total weight and clamp
    (raw / total_weight).clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// Environment configuration
// ---------------------------------------------------------------------------

/// Configuration shared across all RL environments.
///
/// Mirrors relevant fields of Python `HermezAgentEnvConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentConfig {
    /// Maximum number of LLM calls (tool-calling iterations) per rollout.
    pub max_agent_turns: usize,
    /// Sampling temperature for generation during rollouts.
    pub agent_temperature: f64,
    /// System prompt for the agent.
    pub system_prompt: Option<String>,
    /// Max tokens per generation (None for server default).
    pub max_tokens: Option<usize>,
    /// Group size for parallel rollouts.
    pub group_size: usize,
    /// Total training steps.
    pub total_steps: usize,
    /// Steps between evaluations.
    pub steps_per_eval: usize,
    /// Whether to log to wandb (tracked, not actionable in Rust).
    pub use_wandb: bool,
    /// Experiment name for logging.
    pub wandb_name: Option<String>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            max_agent_turns: 30,
            agent_temperature: 1.0,
            system_prompt: None,
            max_tokens: None,
            group_size: 4,
            total_steps: 1000,
            steps_per_eval: 100,
            use_wandb: true,
            wandb_name: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Agent loop configuration (for simulated rollouts)
// ---------------------------------------------------------------------------

/// Configuration for the simulated agent loop used during rollouts.
///
/// In production, this would connect to a real LLM server.
/// For the Rust environment, we provide a mock/simulated agent loop
/// that can be used for testing reward functions and environment logic.
#[derive(Debug, Clone)]
pub struct AgentLoopConfig {
    /// Tool schemas available to the model.
    pub tool_schemas: Vec<serde_json::Value>,
    /// Set of valid tool names the model is allowed to call.
    pub valid_tool_names: std::collections::HashSet<String>,
    /// Maximum turns before stopping.
    pub max_turns: usize,
    /// Unique ID for session isolation.
    pub task_id: String,
    /// Sampling temperature.
    pub temperature: f64,
}

impl Default for AgentLoopConfig {
    fn default() -> Self {
        Self {
            tool_schemas: Vec::new(),
            valid_tool_names: std::collections::HashSet::new(),
            max_turns: 30,
            task_id: uuid::Uuid::new_v4().to_string(),
            temperature: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment trait
// ---------------------------------------------------------------------------

/// Core environment trait, mirroring Python's `HermezAgentBaseEnv`.
///
/// Subclasses implement:
/// - `setup()` — load dataset, initialize state
/// - `get_next_item()` — return the next item from the dataset
/// - `format_prompt()` — convert a dataset item into the user message
/// - `compute_reward()` — score the rollout
/// - `evaluate()` — periodic evaluation on held-out data
///
/// The `run_rollout()` method is provided by default and runs the agent loop
/// + reward computation for a single item.
#[async_trait::async_trait]
pub trait Environment: Send + Sync {
    /// Environment name (e.g., "math", "tool-use", "atropos", "web-research").
    fn name(&self) -> &str;

    /// Load dataset, initialize state. Called once when the environment starts.
    async fn setup(&mut self) -> Result<(), EnvError>;

    /// Return the next item from the dataset for rollout.
    /// Should cycle through the dataset.
    async fn get_next_item(&mut self) -> Result<HashMap<String, serde_json::Value>, EnvError>;

    /// Convert a dataset item into the user message string.
    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String;

    /// Score the rollout. Returns a reward value (typically 0.0 to 1.0).
    async fn compute_reward(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> f64;

    /// Periodic evaluation. Called every `steps_per_eval` steps.
    async fn evaluate(&mut self) -> Result<HashMap<String, f64>, EnvError>;

    /// Get the environment configuration.
    fn config(&self) -> &EnvironmentConfig;

    /// Run a single rollout for the given item.
    /// Default implementation: format prompt, simulate agent result, compute reward.
    ///
    /// In production, this would call an LLM server. For now, we provide
    /// a hook that subclasses can override or use via a simulated agent loop.
    async fn run_rollout(
        &self,
        item: &HashMap<String, serde_json::Value>,
    ) -> Result<ScoredTrajectory, EnvError> {
        // Build initial messages
        let mut messages = Vec::new();
        if let Some(ref sys_prompt) = self.config().system_prompt {
            messages.push(Message::system(sys_prompt));
        }
        messages.push(Message::user(&self.format_prompt(item)));

        // In a real implementation, this would call an LLM server.
        // For the base trait, we create a placeholder result.
        // Subclasses that need real LLM interaction should override this.
        let result = AgentResult::new(messages);

        let reward = self.compute_reward(item, &result).await;

        Ok(ScoredTrajectory {
            item: item.clone(),
            result,
            reward,
            reward_signals: None,
        })
    }

    /// Format trajectories for wandb display (human-readable).
    fn format_trajectory_for_display(messages: &[Message]) -> String {
        let mut parts = Vec::new();
        for msg in messages {
            match msg.role.as_str() {
                "system" => parts.push(format!("[SYSTEM]\n{}", msg.content)),
                "user" => parts.push(format!("[USER]\n{}", msg.content)),
                "assistant" => {
                    if let Some(ref reasoning) = msg.reasoning_content {
                        let truncated = if reasoning.len() > 300 {
                            format!("{}...", &reasoning[..300])
                        } else {
                            reasoning.clone()
                        };
                        parts.push(format!("[ASSISTANT thinking]\n{}", truncated));
                    }
                    if !msg.content.is_empty() {
                        parts.push(format!("[ASSISTANT]\n{}", msg.content));
                    }
                    if let Some(ref tool_calls) = msg.tool_calls {
                        for tc in tool_calls {
                            let args_str = serde_json::to_string(&tc.arguments).unwrap_or_default();
                            let truncated = if args_str.len() > 200 {
                                format!("{}...", &args_str[..200])
                            } else {
                                args_str
                            };
                            parts.push(format!("[TOOL CALL] {}({})", tc.name, truncated));
                        }
                    }
                }
                "tool" => {
                    let truncated = if msg.content.len() > 500 {
                        format!("{}...", &msg.content[..500])
                    } else {
                        msg.content.clone()
                    };
                    parts.push(format!("[TOOL RESULT] {}", truncated));
                }
                _ => {}
            }
        }
        parts.join("\n\n")
    }
}

// ---------------------------------------------------------------------------
// Evaluation result
// ---------------------------------------------------------------------------

/// Aggregated evaluation metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvalMetrics {
    pub metrics: HashMap<String, f64>,
    pub samples: Vec<EvalSample>,
    pub elapsed_secs: f64,
}

/// A single evaluation sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalSample {
    pub prompt: String,
    pub response: String,
    pub expected: String,
    pub correctness: f64,
    pub reward: f64,
}

// ---------------------------------------------------------------------------
// Utility: extract domains from URLs in text
// ---------------------------------------------------------------------------

/// Extract unique domains from URLs cited in text.
pub fn extract_domains(text: &str) -> std::collections::HashSet<String> {
    let mut domains = std::collections::HashSet::new();
    let re = regex::Regex::new(r"https?://[^\s)>]+").unwrap();
    for cap in re.find_iter(text) {
        let url_str = cap.as_str();
        // Trim trailing punctuation
        let url_str = url_str.trim_end_matches(['.', ',', ')']);
        if let Ok(parsed) = url::Url::parse(url_str) {
            let domain = parsed
                .host_str()
                .unwrap_or("")
                .to_lowercase()
                .trim_start_matches("www.")
                .to_string();
            if !domain.is_empty() {
                domains.insert(domain);
            }
        }
    }
    domains
}

// ---------------------------------------------------------------------------
// Utility: heuristic score based on keyword overlap
// ---------------------------------------------------------------------------

/// Lightweight keyword overlap score as fallback for LLM judge.
pub fn heuristic_score(expected: &str, model_answer: &str) -> f64 {
    let stopwords: std::collections::HashSet<&str> = [
        "the", "a", "an", "is", "are", "was", "were", "of", "in", "on",
        "at", "to", "for", "with", "and", "or", "but", "it", "its",
        "this", "that", "as", "by", "from", "be", "has", "have", "had",
    ]
    .into_iter()
    .collect();

    let tokenize = |text: &str| -> std::collections::HashSet<String> {
        regex::Regex::new(r"\b\w+\b")
            .unwrap()
            .find_iter(&text.to_lowercase())
            .map(|m| m.as_str().to_string())
            .filter(|t| !stopwords.contains(t.as_str()) && (t.chars().all(|c| c.is_ascii_digit()) || t.len() > 2))
            .collect()
    };

    let expected_tokens = tokenize(expected);
    let answer_tokens = tokenize(model_answer);

    if expected_tokens.is_empty() {
        return 0.5;
    }

    let overlap = expected_tokens.intersection(&answer_tokens).count() as f64;
    let union = expected_tokens.union(&answer_tokens).count() as f64;

    let jaccard = if union > 0.0 { overlap / union } else { 0.0 };
    let recall = overlap / expected_tokens.len() as f64;

    (0.4 * jaccard + 0.6 * recall).min(1.0)
}

// ---------------------------------------------------------------------------
// Utility: parse judge JSON response
// ---------------------------------------------------------------------------

/// Extract a score from LLM judge JSON response.
pub fn parse_judge_score(text: &str) -> Option<f64> {
    // Try parsing as JSON first
    if let Ok(data) = serde_json::from_str::<serde_json::Value>(text) {
        if let Some(score) = data.get("score").and_then(|v| v.as_f64()) {
            if (0.0..=1.0).contains(&score) {
                return Some(score);
            }
        }
    }

    // Fallback: extract score with regex
    let re = regex::Regex::new(r#""score"\s*:\s*([0-9.]+)"#).unwrap();
    if let Some(caps) = re.captures(text) {
        if let Ok(score) = caps[1].parse::<f64>() {
            if (0.0..=1.0).contains(&score) {
                return Some(score);
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Utility: clean markdown code blocks from text
// ---------------------------------------------------------------------------

/// Remove markdown code block fences from text.
pub fn strip_markdown(text: &str) -> String {
    let re = regex::Regex::new(r"```(?:\w*\n|\n)?([\s\S]*?)```").unwrap();
    re.replace_all(text, "$1").to_string()
}
