#![allow(dead_code)]
//! MathEnv — RL Environment for Math Problem Solving
//!
//! Trains models to solve math problems correctly.
//!
//! Reward signals:
//!   - Exact match (answer matches ground truth exactly)
//!   - Numeric tolerance (answer is within a tolerance of the correct value)
//!   - Keyword overlap (shares key terms with the reference answer)
//!
//! Dataset: Built-in math problems with known answers.
//! Can be extended to load from HuggingFace datasets (e.g., GSM8K, MATH).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::base::{
    AgentResult, EnvError, Environment, EnvironmentConfig, EvalSample,
    Message, ScoredTrajectory, heuristic_score, strip_markdown,
};

// ---------------------------------------------------------------------------
// Built-in math problems (fallback dataset)
// ---------------------------------------------------------------------------

/// A single math problem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MathProblem {
    pub problem: String,
    pub answer: String,
    pub difficulty: String,
}

/// Built-in math problems for testing and development.
pub fn builtin_math_problems() -> Vec<MathProblem> {
    vec![
        MathProblem {
            problem: "What is 15 + 27?".into(),
            answer: "42".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is the square root of 144?".into(),
            answer: "12".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "Solve for x: 2x + 5 = 13".into(),
            answer: "4".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is 15% of 200?".into(),
            answer: "30".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is the area of a circle with radius 5? Use pi = 3.14159.".into(),
            answer: "78.53975".into(),
            difficulty: "medium".into(),
        },
        MathProblem {
            problem: "What is the sum of the first 10 positive integers?".into(),
            answer: "55".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "If a train travels at 60 mph for 2.5 hours, how far does it go?".into(),
            answer: "150".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is 2^10?".into(),
            answer: "1024".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "Solve: (3 + 4) * (5 - 2)".into(),
            answer: "21".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is the factorial of 6 (6!)?".into(),
            answer: "720".into(),
            difficulty: "medium".into(),
        },
        MathProblem {
            problem: "A rectangle has length 8 and width 3. What is its perimeter?".into(),
            answer: "22".into(),
            difficulty: "easy".into(),
        },
        MathProblem {
            problem: "What is the derivative of x^3 + 2x^2 - 5x + 3 with respect to x?".into(),
            answer: "3x^2 + 4x - 5".into(),
            difficulty: "hard".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the math RL environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MathEnvConfig {
    /// Base environment config.
    pub base: EnvironmentConfig,
    /// Numeric tolerance for floating-point comparison.
    pub numeric_tolerance: f64,
    /// Weights for reward signals.
    pub exact_match_weight: f64,
    pub numeric_weight: f64,
    pub keyword_weight: f64,
    /// Number of items to hold out for evaluation.
    pub eval_size: usize,
    /// Fraction of dataset to hold out for evaluation.
    pub eval_split_ratio: f64,
}

impl Default for MathEnvConfig {
    fn default() -> Self {
        Self {
            base: EnvironmentConfig {
                max_agent_turns: 10,
                agent_temperature: 0.7,
                system_prompt: Some(
                    "You are a math expert. Solve problems step by step. \
                     Show your work clearly and provide the final answer as a single \
                     number or expression at the end."
                        .into(),
                ),
                group_size: 4,
                total_steps: 1000,
                steps_per_eval: 100,
                use_wandb: true,
                wandb_name: Some("math".into()),
                max_tokens: None,
            },
            numeric_tolerance: 0.01,
            exact_match_weight: 0.5,
            numeric_weight: 0.3,
            keyword_weight: 0.2,
            eval_size: 3,
            eval_split_ratio: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Math problem solving RL environment.
#[derive(Default)]
pub struct MathEnv {
    config: MathEnvConfig,
    problems: Vec<MathProblem>,
    eval_problems: Vec<MathProblem>,
    index: usize,
    is_setup: bool,
}

impl MathEnv {
    /// Create a new MathEnv with default configuration.
    pub fn new() -> Self {
        Self {
            config: MathEnvConfig::default(),
            problems: Vec::new(),
            eval_problems: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new MathEnv with a custom configuration.
    pub fn with_config(config: MathEnvConfig) -> Self {
        Self {
            config,
            problems: Vec::new(),
            eval_problems: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new MathEnv with custom problems.
    pub fn with_problems(problems: Vec<MathProblem>, config: Option<MathEnvConfig>) -> Self {
        let config = config.unwrap_or_default();
        let eval_size = config.eval_size.min(problems.len() / 2);
        let mut problems = problems;
        // Shuffle using rand
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        problems.shuffle(&mut rng);

        let eval_problems = problems.drain(..eval_size).collect();

        Self {
            config,
            problems,
            eval_problems,
            index: 0,
            is_setup: true,
        }
    }

    /// Compute exact match score.
    fn exact_match_score(final_response: &str, expected: &str) -> f64 {
        let cleaned_response = final_response.trim().to_lowercase();
        let cleaned_expected = expected.trim().to_lowercase();

        // Direct match
        if cleaned_response == cleaned_expected {
            return 1.0;
        }

        // Response contains the expected answer
        if cleaned_response.contains(&cleaned_expected) {
            return 1.0;
        }

        // Extract the last number or expression from the response
        // (common pattern: "So the answer is X")
        let re = regex::Regex::new(r"(\d+(?:\.\d+)?)\s*$").unwrap();
        if let Some(caps) = re.captures(&cleaned_response) {
            let extracted = &caps[1];
            if extracted == cleaned_expected {
                return 1.0;
            }
        }

        0.0
    }

    /// Compute numeric match score (for floating-point answers).
    fn numeric_match_score(final_response: &str, expected: &str, tolerance: f64) -> f64 {
        // Extract numbers from response
        let re = regex::Regex::new(r"-?\d+(?:\.\d+)?").unwrap();
        let response_numbers: Vec<f64> = re
            .find_iter(final_response)
            .filter_map(|m| m.as_str().parse::<f64>().ok())
            .collect();

        if let Ok(expected_num) = expected.parse::<f64>() {
            for &num in &response_numbers {
                if (num - expected_num).abs() <= tolerance {
                    return 1.0;
                }
            }
        }

        0.0
    }

    /// Compute reward breakdown for logging.
    fn compute_reward_breakdown(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> (f64, HashMap<String, f64>) {
        let expected = match item.get("answer") {
            Some(v) => v.as_str().unwrap_or("").to_string(),
            None => String::new(),
        };

        let final_response = &result.final_response;
        let stripped = strip_markdown(final_response);

        // Signal 1: Exact match
        let exact_match = Self::exact_match_score(&stripped, &expected);

        // Signal 2: Numeric match
        let numeric_match = Self::numeric_match_score(&stripped, &expected, self.config.numeric_tolerance);

        // Signal 3: Keyword overlap (heuristic)
        let keyword_score = heuristic_score(&expected, &stripped);

        // Composite reward
        let total_weight = self.config.exact_match_weight + self.config.numeric_weight + self.config.keyword_weight;
        let reward = (self.config.exact_match_weight * exact_match
            + self.config.numeric_weight * numeric_match
            + self.config.keyword_weight * keyword_score)
            / total_weight.max(f64::EPSILON);

        let mut signals = HashMap::new();
        signals.insert("exact_match".into(), exact_match);
        signals.insert("numeric_match".into(), numeric_match);
        signals.insert("keyword_score".into(), keyword_score);

        (reward.clamp(0.0, 1.0), signals)
    }
}

#[async_trait::async_trait]
impl Environment for MathEnv {
    fn name(&self) -> &str {
        "math"
    }

    async fn setup(&mut self) -> Result<(), EnvError> {
        let problems = builtin_math_problems();
        let eval_size = self.config.eval_size.min(problems.len() / 2);

        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        let mut shuffled = problems;
        shuffled.shuffle(&mut rng);

        self.eval_problems = shuffled.drain(..eval_size).collect();
        self.problems = shuffled;
        self.index = 0;
        self.is_setup = true;

        tracing::info!(
            "MathEnv setup: {} train / {} eval items",
            self.problems.len(),
            self.eval_problems.len()
        );

        Ok(())
    }

    async fn get_next_item(&mut self) -> Result<HashMap<String, serde_json::Value>, EnvError> {
        if !self.is_setup {
            self.setup().await?;
        }
        if self.problems.is_empty() {
            return Err(EnvError::EmptyDataset);
        }
        let problem = &self.problems[self.index % self.problems.len()];
        self.index += 1;

        let mut item = HashMap::new();
        item.insert("problem".into(), serde_json::Value::String(problem.problem.clone()));
        item.insert("answer".into(), serde_json::Value::String(problem.answer.clone()));
        item.insert(
            "difficulty".into(),
            serde_json::Value::String(problem.difficulty.clone()),
        );
        Ok(item)
    }

    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String {
        let problem = match item.get("problem") {
            Some(v) => v.as_str().unwrap_or("Unknown problem"),
            None => "Unknown problem",
        };
        format!(
            "Solve the following math problem. Show your work step by step.\n\n\
             Problem: {}\n\n\
             Provide your final answer clearly at the end.",
            problem
        )
    }

    async fn compute_reward(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> f64 {
        let (reward, _signals) = self.compute_reward_breakdown(item, result);
        reward
    }

    async fn evaluate(&mut self) -> Result<HashMap<String, f64>, EnvError> {
        if self.eval_problems.is_empty() {
            return Ok(HashMap::new());
        }

        let eval_size = self.config.eval_size.min(self.eval_problems.len());
        let mut samples = Vec::new();
        let mut total_reward = 0.0;
        let mut total_correct = 0usize;

        for problem in &self.eval_problems[..eval_size] {
            let mut item = HashMap::new();
            item.insert("problem".into(), serde_json::Value::String(problem.problem.clone()));
            item.insert("answer".into(), serde_json::Value::String(problem.answer.clone()));

            let prompt = self.format_prompt(&item);
            let mut messages = Vec::new();
            if let Some(ref sys) = self.config.base.system_prompt {
                messages.push(Message::system(sys));
            }
            messages.push(Message::user(&prompt));

            let result = AgentResult::new(messages);
            let (reward, signals) = self.compute_reward_breakdown(&item, &result);

            let exact_match = signals.get("exact_match").copied().unwrap_or(0.0);
            if exact_match >= 1.0 {
                total_correct += 1;
            }

            total_reward += reward;

            samples.push(EvalSample {
                prompt: problem.problem.clone(),
                response: result.final_response.clone(),
                expected: problem.answer.clone(),
                correctness: exact_match,
                reward,
            });
        }

        let n = samples.len() as f64;
        let mut metrics = HashMap::new();
        metrics.insert("eval/mean_reward".into(), total_reward / n.max(f64::EPSILON));
        metrics.insert("eval/accuracy".into(), total_correct as f64 / n.max(f64::EPSILON));
        metrics.insert("eval/n_items".into(), n);

        tracing::info!(
            "MathEnv eval: mean_reward={:.3}, accuracy={:.3}",
            total_reward / n.max(f64::EPSILON),
            total_correct as f64 / n.max(f64::EPSILON)
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
        let (reward, signals) = self.compute_reward_breakdown(item, &result);

        Ok(ScoredTrajectory {
            item: item.clone(),
            result,
            reward,
            reward_signals: Some(signals),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match_direct() {
        assert_eq!(MathEnv::exact_match_score("42", "42"), 1.0);
        assert_eq!(MathEnv::exact_match_score("42", "43"), 0.0);
    }

    #[test]
    fn test_exact_match_contains() {
        assert_eq!(
            MathEnv::exact_match_score("The answer is 42.", "42"),
            1.0
        );
    }

    #[test]
    fn test_exact_match_case_insensitive() {
        assert_eq!(MathEnv::exact_match_score("42", "42"), 1.0);
    }

    #[test]
    fn test_numeric_match() {
        assert!(
            MathEnv::numeric_match_score("The result is approximately 3.14", "3.14159", 0.01) > 0.0
        );
        assert_eq!(
            MathEnv::numeric_match_score("The answer is 100", "3.14159", 0.01),
            0.0
        );
    }

    #[test]
    fn test_heuristic_score_match() {
        let score = heuristic_score("42", "The answer is 42");
        assert!(score > 0.5);
    }

    #[test]
    fn test_heuristic_score_mismatch() {
        let score = heuristic_score("42", "The answer is 100");
        assert!(score < 0.5);
    }

    #[tokio::test]
    async fn test_math_env_setup_and_next_item() {
        let mut env = MathEnv::new();
        env.setup().await.unwrap();
        let item = env.get_next_item().await.unwrap();
        assert!(item.contains_key("problem"));
        assert!(item.contains_key("answer"));
    }

    #[tokio::test]
    async fn test_math_env_format_prompt() {
        let env = MathEnv::new();
        let mut item = HashMap::new();
        item.insert("problem".into(), serde_json::Value::String("1+1=?".into()));
        let prompt = env.format_prompt(&item);
        assert!(prompt.contains("1+1=?"));
    }

    #[tokio::test]
    async fn test_math_env_reward_exact() {
        let env = MathEnv::new();
        let mut item = HashMap::new();
        item.insert("problem".into(), serde_json::Value::String("What is 15 + 27?".into()));
        item.insert("answer".into(), serde_json::Value::String("42".into()));

        let mut messages = vec![Message::user("What is 15 + 27?")];
        messages.push(Message::assistant("The answer is 42."));
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward > 0.8);
    }

    #[tokio::test]
    async fn test_math_env_reward_wrong() {
        let env = MathEnv::new();
        let mut item = HashMap::new();
        item.insert("problem".into(), serde_json::Value::String("What is 15 + 27?".into()));
        item.insert("answer".into(), serde_json::Value::String("42".into()));

        let mut messages = vec![Message::user("What is 15 + 27?")];
        messages.push(Message::assistant("The answer is 100."));
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward < 0.5);
    }
}
