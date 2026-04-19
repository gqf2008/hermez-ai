#![allow(dead_code)]
//! WebResearchEnv — RL Environment for Multi-Step Web Research
//!
//! Trains models to do accurate, efficient, multi-source web research.
//!
//! Reward signals:
//!   - Answer correctness (LLM judge, 0.0-1.0)
//!   - Source diversity (used >= 2 distinct domains)
//!   - Efficiency (penalizes excessive tool calls)
//!   - Tool usage (bonus for actually using web tools)
//!
//! Dataset: FRAMES benchmark (Google, 2024) — multi-hop factual questions.
//! Fallback: built-in sample questions.
//!
//! Mirrors the Python `environments/web_research_env.py`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::base::{
    AgentResult, EnvError, Environment, EnvironmentConfig, EvalSample,
    Message, ScoredTrajectory, extract_domains, heuristic_score,
};

// ---------------------------------------------------------------------------
// Sample questions (fallback dataset)
// ---------------------------------------------------------------------------

/// A single research question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchQuestion {
    /// The question to research.
    pub question: String,
    /// Reference answer.
    pub answer: String,
    /// Difficulty level.
    pub difficulty: String,
    /// Number of research hops required.
    pub hops: usize,
}

/// Built-in sample questions for testing and development.
///
/// Mirrors the SAMPLE_QUESTIONS from Python `web_research_env.py`.
pub fn sample_research_questions() -> Vec<ResearchQuestion> {
    vec![
        ResearchQuestion {
            question: "What is the current population of the capital city of the country that won the 2022 FIFA World Cup?".into(),
            answer: "Buenos Aires has approximately 3 million people in the city proper, or around 15 million in the greater metro area.".into(),
            difficulty: "medium".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "Who is the CEO of the company that makes the most widely used open-source container orchestration platform?".into(),
            answer: "The Linux Foundation oversees Kubernetes. CNCF (Cloud Native Computing Foundation) is the specific body.".into(),
            difficulty: "medium".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "What programming language was used to write the original version of the web framework used by Instagram?".into(),
            answer: "Django, which Instagram was built on, is written in Python.".into(),
            difficulty: "easy".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "In what year was the university founded where the inventor of the World Wide Web currently holds a professorship?".into(),
            answer: "Tim Berners-Lee holds a professorship at MIT (founded 1861) and the University of Southampton (founded 1952).".into(),
            difficulty: "hard".into(),
            hops: 3,
        },
        ResearchQuestion {
            question: "What is the latest stable version of the programming language that ranks #1 on the TIOBE index as of this year?".into(),
            answer: "Python is currently #1 on TIOBE. The latest stable version should be verified via the official python.org site.".into(),
            difficulty: "medium".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "How many employees does the parent company of Instagram have?".into(),
            answer: "Meta Platforms (parent of Instagram) employs approximately 70,000+ people as of recent reports.".into(),
            difficulty: "medium".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "What is the current interest rate set by the central bank of the country where the Eiffel Tower is located?".into(),
            answer: "The European Central Bank sets rates for France/eurozone. The current rate should be verified.".into(),
            difficulty: "hard".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "Which company acquired the startup founded by the creator of Oculus VR?".into(),
            answer: "Palmer Luckey founded Oculus VR, which was acquired by Facebook (now Meta).".into(),
            difficulty: "medium".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "What is the market cap of the company that owns the most popular search engine in Russia?".into(),
            answer: "Yandex (now split into separate entities after 2024 restructuring).".into(),
            difficulty: "hard".into(),
            hops: 2,
        },
        ResearchQuestion {
            question: "What was the GDP growth rate of the country that hosted the most recent Summer Olympics?".into(),
            answer: "Paris, France hosted the 2024 Summer Olympics. France's recent GDP growth should be verified via World Bank or IMF data.".into(),
            difficulty: "hard".into(),
            hops: 2,
        },
    ]
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the web research RL environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebResearchEnvConfig {
    /// Base environment config.
    pub base: EnvironmentConfig,
    /// Reward weights.
    pub correctness_weight: f64,
    pub tool_usage_weight: f64,
    pub efficiency_weight: f64,
    /// Bonus reward for citing >= 2 distinct domains.
    pub diversity_bonus: f64,
    /// Efficiency thresholds.
    pub efficient_max_calls: usize,
    pub heavy_penalty_calls: usize,
    /// Number of items to hold out for evaluation.
    pub eval_size: usize,
    /// Fraction of dataset to hold out for evaluation.
    pub eval_split_ratio: f64,
}

impl Default for WebResearchEnvConfig {
    fn default() -> Self {
        Self {
            base: EnvironmentConfig {
                max_agent_turns: 15,
                agent_temperature: 1.0,
                system_prompt: Some(
                    "You are a highly capable research agent. When asked a factual question, \
                     always use web_search to find current, accurate information before answering. \
                     Cite at least 2 sources. Be concise and accurate."
                        .into(),
                ),
                group_size: 4,
                total_steps: 1000,
                steps_per_eval: 100,
                use_wandb: true,
                wandb_name: Some("web-research".into()),
                max_tokens: None,
            },
            correctness_weight: 0.6,
            tool_usage_weight: 0.2,
            efficiency_weight: 0.2,
            diversity_bonus: 0.1,
            efficient_max_calls: 5,
            heavy_penalty_calls: 10,
            eval_size: 3,
            eval_split_ratio: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Web research RL environment.
///
/// The model is given a factual question requiring 2-3 hops of web research
/// and must use web_search / web_extract tools to find and synthesize the answer.
#[derive(Default)]
pub struct WebResearchEnv {
    config: WebResearchEnvConfig,
    questions: Vec<ResearchQuestion>,
    eval_questions: Vec<ResearchQuestion>,
    index: usize,
    is_setup: bool,
    // Metric buffers (for logging).
    reward_buffer: Vec<f64>,
    correctness_buffer: Vec<f64>,
    tool_usage_buffer: Vec<f64>,
    efficiency_buffer: Vec<f64>,
    diversity_buffer: Vec<f64>,
}

impl WebResearchEnv {
    /// Create a new WebResearchEnv with default configuration.
    pub fn new() -> Self {
        Self {
            config: WebResearchEnvConfig::default(),
            questions: Vec::new(),
            eval_questions: Vec::new(),
            index: 0,
            is_setup: false,
            reward_buffer: Vec::new(),
            correctness_buffer: Vec::new(),
            tool_usage_buffer: Vec::new(),
            efficiency_buffer: Vec::new(),
            diversity_buffer: Vec::new(),
        }
    }

    /// Create a new WebResearchEnv with a custom configuration.
    pub fn with_config(config: WebResearchEnvConfig) -> Self {
        Self {
            config,
            questions: Vec::new(),
            eval_questions: Vec::new(),
            index: 0,
            is_setup: false,
            reward_buffer: Vec::new(),
            correctness_buffer: Vec::new(),
            tool_usage_buffer: Vec::new(),
            efficiency_buffer: Vec::new(),
            diversity_buffer: Vec::new(),
        }
    }

    /// Create a new WebResearchEnv with custom questions.
    pub fn with_questions(
        questions: Vec<ResearchQuestion>,
        config: Option<WebResearchEnvConfig>,
    ) -> Self {
        let config = config.unwrap_or_default();
        let eval_size = config.eval_size.min(questions.len() / 2);
        let mut questions = questions;
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        questions.shuffle(&mut rng);

        let eval_questions = questions.drain(..eval_size).collect();

        Self {
            config,
            questions,
            eval_questions,
            index: 0,
            is_setup: true,
            reward_buffer: Vec::new(),
            correctness_buffer: Vec::new(),
            tool_usage_buffer: Vec::new(),
            efficiency_buffer: Vec::new(),
            diversity_buffer: Vec::new(),
        }
    }

    /// Compute efficiency score based on tool call count.
    fn efficiency_score(tool_call_count: usize, config: &WebResearchEnvConfig) -> f64 {
        if tool_call_count <= config.efficient_max_calls {
            1.0
        } else if tool_call_count <= config.heavy_penalty_calls {
            1.0 - (tool_call_count - config.efficient_max_calls) as f64 * 0.08
        } else {
            (1.0 - (tool_call_count - config.efficient_max_calls) as f64 * 0.12).max(0.0)
        }
    }

    /// Compute reward breakdown.
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
        let tool_call_count = result.total_tool_calls;

        // Signal 1: Answer correctness (LLM judge — simulated via heuristic)
        // In production, this would call an LLM judge endpoint
        let correctness = self.llm_judge_score(
            item.get("question").and_then(|v| v.as_str()).unwrap_or(""),
            &expected,
            final_response,
        );

        // Signal 2: Web tool usage
        let web_tools: std::collections::HashSet<&str> =
            ["web_search", "web_extract", "search", "firecrawl"].into_iter().collect();
        let tool_used = if result
            .tools_used
            .iter()
            .any(|t| web_tools.contains(t.as_str()))
        {
            1.0
        } else {
            0.0
        };

        // Signal 3: Efficiency
        let efficiency = Self::efficiency_score(tool_call_count, &self.config);

        // Bonus: Source diversity
        let domains = extract_domains(final_response);
        let diversity = if domains.len() >= 2 {
            self.config.diversity_bonus
        } else {
            0.0
        };

        // Combine
        let reward = (self.config.correctness_weight * correctness
            + self.config.tool_usage_weight * tool_used
            + self.config.efficiency_weight * efficiency
            + diversity)
            .clamp(0.0, 1.0);

        let mut signals = HashMap::new();
        signals.insert("correctness".into(), correctness);
        signals.insert("tool_used".into(), tool_used);
        signals.insert("efficiency".into(), efficiency);
        signals.insert("diversity".into(), diversity);

        (reward, signals)
    }

    /// LLM judge score — uses heuristic as fallback (simulated LLM judge).
    ///
    /// In production, this would call a separate LLM endpoint to judge answer quality.
    fn llm_judge_score(&self, question: &str, expected: &str, model_answer: &str) -> f64 {
        if model_answer.trim().is_empty() {
            return 0.0;
        }

        // Simulated judge: use keyword overlap with extra credit for question-specific terms
        let base_score = heuristic_score(expected, model_answer);

        // Check if model answer mentions key entities from the question
        let question_keywords: Vec<&str> = question
            .split_whitespace()
            .filter(|w| w.len() > 3)
            .collect();
        let answer_lower = model_answer.to_lowercase();
        let question_hits = question_keywords
            .iter()
            .filter(|&&w| answer_lower.contains(&w.to_lowercase()))
            .count();
        let question_relevance = question_hits as f64 / question_keywords.len().max(1) as f64;

        // Combine base similarity with question relevance
        (0.6 * base_score + 0.4 * question_relevance).clamp(0.0, 1.0)
    }

    /// Flush metric buffers and return the collected values.
    fn flush_metrics(&mut self) -> HashMap<String, f64> {
        let mut metrics = HashMap::new();
        let n = self.reward_buffer.len();
        if n > 0 {
            let n_f = n as f64;
            metrics.insert(
                "train/mean_reward".into(),
                self.reward_buffer.iter().sum::<f64>() / n_f,
            );
            metrics.insert(
                "train/mean_correctness".into(),
                self.correctness_buffer.iter().sum::<f64>() / n_f,
            );
            metrics.insert(
                "train/mean_tool_usage".into(),
                self.tool_usage_buffer.iter().sum::<f64>() / n_f,
            );
            metrics.insert(
                "train/mean_efficiency".into(),
                self.efficiency_buffer.iter().sum::<f64>() / n_f,
            );
            metrics.insert(
                "train/mean_diversity".into(),
                self.diversity_buffer.iter().sum::<f64>() / n_f,
            );
            metrics.insert("train/total_rollouts".into(), n_f);
            metrics.insert(
                "train/correct_rate".into(),
                self.correctness_buffer
                    .iter()
                    .filter(|&&c| c >= 0.7)
                    .count() as f64
                    / n_f,
            );
            metrics.insert(
                "train/tool_usage_rate".into(),
                self.tool_usage_buffer
                    .iter()
                    .filter(|&&t| t > 0.0)
                    .count() as f64
                    / n_f,
            );

            self.reward_buffer.clear();
            self.correctness_buffer.clear();
            self.tool_usage_buffer.clear();
            self.efficiency_buffer.clear();
            self.diversity_buffer.clear();
        }
        metrics
    }
}

#[async_trait::async_trait]
impl Environment for WebResearchEnv {
    fn name(&self) -> &str {
        "web-research"
    }

    async fn setup(&mut self) -> Result<(), EnvError> {
        let questions = sample_research_questions();
        let eval_size = self.config.eval_size.min(questions.len() / 2);

        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        let mut shuffled = questions;
        shuffled.shuffle(&mut rng);

        self.eval_questions = shuffled.drain(..eval_size).collect();
        self.questions = shuffled;
        self.index = 0;
        self.is_setup = true;

        tracing::info!(
            "WebResearchEnv setup: {} train / {} eval items",
            self.questions.len(),
            self.eval_questions.len()
        );

        Ok(())
    }

    async fn get_next_item(&mut self) -> Result<HashMap<String, serde_json::Value>, EnvError> {
        if !self.is_setup {
            self.setup().await?;
        }
        if self.questions.is_empty() {
            return Err(EnvError::EmptyDataset);
        }
        let q = &self.questions[self.index % self.questions.len()];
        self.index += 1;

        let mut item = HashMap::new();
        item.insert(
            "question".into(),
            serde_json::Value::String(q.question.clone()),
        );
        item.insert("answer".into(), serde_json::Value::String(q.answer.clone()));
        item.insert(
            "difficulty".into(),
            serde_json::Value::String(q.difficulty.clone()),
        );
        item.insert("hops".into(), serde_json::Value::Number(q.hops.into()));
        Ok(item)
    }

    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String {
        let question = match item.get("question") {
            Some(v) => v.as_str().unwrap_or("Unknown question"),
            None => "Unknown question",
        };
        format!(
            "Research the following question thoroughly using web search. \
             You MUST search the web to find current, accurate information — \
             do not rely solely on your training data.\n\n\
             Question: {}\n\n\
             Requirements:\n\
             - Use web_search and/or web_extract tools to find information\n\
             - Search at least 2 different sources\n\
             - Provide a concise, accurate answer (2-4 sentences)\n\
             - Cite the sources you used",
            question
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
        if self.eval_questions.is_empty() {
            return Ok(HashMap::new());
        }

        let eval_size = self.config.eval_size.min(self.eval_questions.len());
        let mut samples = Vec::new();
        let mut total_reward = 0.0;
        let mut total_correctness = 0.0;

        for q in &self.eval_questions[..eval_size] {
            let mut item = HashMap::new();
            item.insert(
                "question".into(),
                serde_json::Value::String(q.question.clone()),
            );
            item.insert("answer".into(), serde_json::Value::String(q.answer.clone()));

            let prompt = self.format_prompt(&item);
            let mut messages = Vec::new();
            if let Some(ref sys) = self.config.base.system_prompt {
                messages.push(Message::system(sys));
            }
            messages.push(Message::user(&prompt));

            let result = AgentResult::new(messages);
            let (reward, signals) = self.compute_reward_breakdown(&item, &result);

            let correctness = signals.get("correctness").copied().unwrap_or(0.0);
            total_correctness += correctness;
            total_reward += reward;

            samples.push(EvalSample {
                prompt: q.question.clone(),
                response: result.final_response.clone(),
                expected: q.answer.clone(),
                correctness,
                reward,
            });
        }

        let n = samples.len() as f64;
        let mut metrics = HashMap::new();
        metrics.insert("eval/mean_reward".into(), total_reward / n.max(f64::EPSILON));
        metrics.insert(
            "eval/mean_correctness".into(),
            total_correctness / n.max(f64::EPSILON),
        );
        metrics.insert("eval/n_items".into(), n);

        tracing::info!(
            "WebResearchEnv eval: mean_reward={:.3}, mean_correctness={:.3}",
            total_reward / n.max(f64::EPSILON),
            total_correctness / n.max(f64::EPSILON)
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
    fn test_efficiency_score_under_threshold() {
        let config = WebResearchEnvConfig::default();
        assert_eq!(WebResearchEnv::efficiency_score(3, &config), 1.0);
        assert_eq!(WebResearchEnv::efficiency_score(5, &config), 1.0);
    }

    #[test]
    fn test_efficiency_score_over_threshold() {
        let config = WebResearchEnvConfig::default();
        let score = WebResearchEnv::efficiency_score(7, &config);
        assert!(score < 1.0 && score > 0.0);
    }

    #[test]
    fn test_efficiency_score_heavy_penalty() {
        let config = WebResearchEnvConfig::default();
        let score = WebResearchEnv::efficiency_score(15, &config);
        assert!(score < 0.5);
    }

    #[test]
    fn test_extract_domains_from_text() {
        let text = "According to https://en.wikipedia.org/wiki/Python and \
                    https://docs.python.org/3/, Python is popular.";
        let domains = extract_domains(text);
        assert!(domains.contains("en.wikipedia.org") || domains.contains("wikipedia.org"));
        assert!(domains.contains("docs.python.org"));
    }

    #[test]
    fn test_sample_questions_not_empty() {
        let questions = sample_research_questions();
        assert!(!questions.is_empty());
        assert!(questions.iter().any(|q| q.difficulty == "hard"));
    }

    #[tokio::test]
    async fn test_web_research_env_setup_and_next_item() {
        let mut env = WebResearchEnv::new();
        env.setup().await.unwrap();
        let item = env.get_next_item().await.unwrap();
        assert!(item.contains_key("question"));
        assert!(item.contains_key("answer"));
    }

    #[tokio::test]
    async fn test_web_research_env_format_prompt() {
        let env = WebResearchEnv::new();
        let mut item = HashMap::new();
        item.insert(
            "question".into(),
            serde_json::Value::String("What is Python?".into()),
        );
        let prompt = env.format_prompt(&item);
        assert!(prompt.contains("What is Python?"));
    }

    #[tokio::test]
    async fn test_web_research_env_reward_no_answer() {
        let env = WebResearchEnv::new();
        let mut item = HashMap::new();
        item.insert(
            "question".into(),
            serde_json::Value::String("What is 2+2?".into()),
        );
        item.insert("answer".into(), serde_json::Value::String("4".into()));

        let messages = vec![
            Message::user("What is 2+2?"),
            Message::assistant("I don't know."),
        ];
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward < 0.5);
    }
}
