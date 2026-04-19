#![allow(dead_code)]
//! AtroposEnv — Code Generation RL Environment with Test Verification
//!
//! Trains models to write correct code that passes test suites.
//!
//! Reward signals:
//!   - Test pass rate (fraction of tests that pass)
//!   - Code quality heuristic (indentation, length, syntax checks)
//!   - LLM judge score (quality of explanation / approach)
//!
//! Dataset: Built-in coding tasks with test code.
//! Can be extended to load from HuggingFace datasets (e.g., HumanEval, MBPP).
//!
//! Inspired by the Python `environments/agentic_opd_env.py`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::base::{
    AgentResult, EnvError, Environment, EnvironmentConfig, EvalSample,
    Message, ScoredTrajectory, heuristic_score, strip_markdown,
};

// ---------------------------------------------------------------------------
// Coding tasks
// ---------------------------------------------------------------------------

/// A single coding task with test verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodingTask {
    /// Task description.
    pub task: String,
    /// Test code to verify the solution (Python).
    pub test_code: String,
    /// Difficulty level.
    pub difficulty: String,
}

/// Built-in coding tasks for testing and development.
///
/// Mirrors the BUILTIN_CODING_TASKS from Python `agentic_opd_env.py`.
pub fn builtin_coding_tasks() -> Vec<CodingTask> {
    vec![
        CodingTask {
            task: "Write a Python function `fizzbuzz(n)` that returns a list of strings from 1 to n. \
                   For multiples of 3 return 'Fizz', for multiples of 5 return 'Buzz', \
                   for multiples of both return 'FizzBuzz', otherwise the number as a string.".into(),
            test_code: "from solution import fizzbuzz\n\
                        assert fizzbuzz(15) == ['1','2','Fizz','4','Buzz','Fizz','7','8','Fizz','Buzz','11','Fizz','13','14','FizzBuzz']\n\
                        assert fizzbuzz(1) == ['1']\n\
                        assert fizzbuzz(0) == []\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "easy".into(),
        },
        CodingTask {
            task: "Write a Python function `is_palindrome(s)` that checks if a string is a palindrome, \
                   ignoring case and non-alphanumeric characters. Return True or False.".into(),
            test_code: "from solution import is_palindrome\n\
                        assert is_palindrome('A man, a plan, a canal: Panama') == True\n\
                        assert is_palindrome('race a car') == False\n\
                        assert is_palindrome('') == True\n\
                        assert is_palindrome('Was it a car or a cat I saw?') == True\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "easy".into(),
        },
        CodingTask {
            task: "Write a Python function `two_sum(nums, target)` that returns the indices of the two \
                   numbers in `nums` that add up to `target`. Assume exactly one solution exists.".into(),
            test_code: "from solution import two_sum\n\
                        assert two_sum([2, 7, 11, 15], 9) == [0, 1]\n\
                        assert two_sum([3, 2, 4], 6) == [1, 2]\n\
                        assert two_sum([3, 3], 6) == [0, 1]\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "easy".into(),
        },
        CodingTask {
            task: "Write a Python function `flatten(lst)` that takes an arbitrarily nested list and \
                   returns a flat list of all elements.".into(),
            test_code: "from solution import flatten\n\
                        assert flatten([1, [2, [3, 4], 5]]) == [1, 2, 3, 4, 5]\n\
                        assert flatten([]) == []\n\
                        assert flatten([1, 2, 3]) == [1, 2, 3]\n\
                        assert flatten([[[[1]]]]) == [1]\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "medium".into(),
        },
        CodingTask {
            task: "Write a Python function `longest_common_prefix(strs)` that finds the longest \
                   common prefix string amongst a list of strings.".into(),
            test_code: "from solution import longest_common_prefix\n\
                        assert longest_common_prefix(['flower', 'flow', 'flight']) == 'fl'\n\
                        assert longest_common_prefix(['dog', 'racecar', 'car']) == ''\n\
                        assert longest_common_prefix(['interspecies', 'interstellar', 'interstate']) == 'inters'\n\
                        assert longest_common_prefix(['a']) == 'a'\n\
                        assert longest_common_prefix([]) == ''\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "easy".into(),
        },
        CodingTask {
            task: "Write a Python function `group_anagrams(strs)` that groups anagrams together. \
                   Return a list of lists.".into(),
            test_code: "from solution import group_anagrams\n\
                        result = group_anagrams(['eat', 'tea', 'tan', 'ate', 'nat', 'bat'])\n\
                        result_sorted = sorted([sorted(g) for g in result])\n\
                        assert result_sorted == [['ate', 'eat', 'tea'], ['bat'], ['nat', 'tan']]\n\
                        assert group_anagrams([]) == []\n\
                        assert group_anagrams(['a']) == [['a']]\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "medium".into(),
        },
        CodingTask {
            task: "Write a Python function `valid_parentheses(s)` that determines if a string \
                   containing just parentheses is valid.".into(),
            test_code: "from solution import valid_parentheses\n\
                        assert valid_parentheses('()') == True\n\
                        assert valid_parentheses('()[]{}') == True\n\
                        assert valid_parentheses('(]') == False\n\
                        assert valid_parentheses('([)]') == False\n\
                        assert valid_parentheses('{[]}') == True\n\
                        assert valid_parentheses('') == True\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "easy".into(),
        },
        CodingTask {
            task: "Write a Python function `merge_intervals(intervals)` that merges overlapping \
                   intervals. Return a list of non-overlapping intervals.".into(),
            test_code: "from solution import merge_intervals\n\
                        assert merge_intervals([[1,3],[2,6],[8,10],[15,18]]) == [[1,6],[8,10],[15,18]]\n\
                        assert merge_intervals([[1,4],[4,5]]) == [[1,5]]\n\
                        assert merge_intervals([[1,4],[0,4]]) == [[0,4]]\n\
                        assert merge_intervals([]) == []\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "medium".into(),
        },
        CodingTask {
            task: "Write a Python function `unique_paths(m, n)` that counts the number of unique \
                   paths from top-left to bottom-right of an m x n grid, moving only right or down.".into(),
            test_code: "from solution import unique_paths\n\
                        assert unique_paths(3, 7) == 28\n\
                        assert unique_paths(3, 2) == 3\n\
                        assert unique_paths(1, 1) == 1\n\
                        assert unique_paths(7, 3) == 28\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "medium".into(),
        },
        CodingTask {
            task: "Write a Python function `is_valid_sudoku(board)` that checks if a 9x9 Sudoku \
                   board is valid. Empty cells are represented by '.'.".into(),
            test_code: "from solution import is_valid_sudoku\n\
                        board = [['5','3','.','.','7','.','.','.','.'],\n\
                                 ['6','.','.','1','9','5','.','.','.'],\n\
                                 ['.','9','8','.','.','.','.','6','.'],\n\
                                 ['8','.','.','.','6','.','.','.','3'],\n\
                                 ['4','.','.','8','.','3','.','.','1'],\n\
                                 ['7','.','.','.','2','.','.','.','6'],\n\
                                 ['.','6','.','.','.','.','2','8','.'],\n\
                                 ['.','.','.','4','1','9','.','.','5'],\n\
                                 ['.','.','.','.','8','.','.','7','9']]\n\
                        assert is_valid_sudoku(board) == True\n\
                        print('All tests passed!')\n"
                .into(),
            difficulty: "hard".into(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the code generation RL environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtroposEnvConfig {
    /// Base environment config.
    pub base: EnvironmentConfig,
    /// Reward weights.
    pub test_pass_weight: f64,
    pub code_quality_weight: f64,
    pub llm_judge_weight: f64,
    /// Number of items to hold out for evaluation.
    pub eval_size: usize,
    /// Fraction of dataset to hold out for evaluation.
    pub eval_split_ratio: f64,
}

impl Default for AtroposEnvConfig {
    fn default() -> Self {
        Self {
            base: EnvironmentConfig {
                max_agent_turns: 20,
                agent_temperature: 0.8,
                system_prompt: Some(
                    "You are an expert Python programmer. Write correct, clean, efficient code. \
                     Include the function definition exactly as specified. \
                     Make sure the code passes the provided tests."
                        .into(),
                ),
                group_size: 4,
                total_steps: 1000,
                steps_per_eval: 100,
                use_wandb: true,
                wandb_name: Some("atropos".into()),
                max_tokens: None,
            },
            test_pass_weight: 0.6,
            code_quality_weight: 0.2,
            llm_judge_weight: 0.2,
            eval_size: 3,
            eval_split_ratio: 0.2,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// Code generation RL environment with test verification.
///
/// The model is given a coding task and must write a Python function that
/// passes the provided test suite.
#[derive(Default)]
pub struct AtroposEnv {
    config: AtroposEnvConfig,
    tasks: Vec<CodingTask>,
    eval_tasks: Vec<CodingTask>,
    index: usize,
    is_setup: bool,
}

impl AtroposEnv {
    /// Create a new AtroposEnv with default configuration.
    pub fn new() -> Self {
        Self {
            config: AtroposEnvConfig::default(),
            tasks: Vec::new(),
            eval_tasks: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new AtroposEnv with a custom configuration.
    pub fn with_config(config: AtroposEnvConfig) -> Self {
        Self {
            config,
            tasks: Vec::new(),
            eval_tasks: Vec::new(),
            index: 0,
            is_setup: false,
        }
    }

    /// Create a new AtroposEnv with custom tasks.
    pub fn with_tasks(tasks: Vec<CodingTask>, config: Option<AtroposEnvConfig>) -> Self {
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

    /// Extract Python code from the model's response.
    fn extract_python_code(response: &str) -> String {
        let stripped = strip_markdown(response);

        // Try to find a Python code block
        let re = regex::Regex::new(r"(?s)```python\s*\n(.*?)```").unwrap();
        if let Some(caps) = re.captures(&stripped) {
            return caps[1].to_string();
        }

        // Try generic code block
        let re2 = regex::Regex::new(r"(?s)```\s*\n(.*?)```").unwrap();
        if let Some(caps) = re2.captures(&stripped) {
            let content = caps[1].trim();
            // Heuristic: if it looks like Python code
            if content.contains("def ") || content.contains("class ") || content.contains("import ") {
                return content.to_string();
            }
        }

        // Fallback: return the whole response
        stripped
    }

    /// Compute a code quality heuristic score.
    fn code_quality_score(code: &str) -> f64 {
        if code.trim().is_empty() {
            return 0.0;
        }

        let mut score = 0.0;

        // Has function definitions
        let func_count = regex::Regex::new(r"^\s*def ")
            .unwrap()
            .find_iter(code)
            .count();
        if func_count > 0 {
            score += 0.4;
        }

        // Has proper indentation (multiple lines with indent)
        let lines: Vec<&str> = code.lines().collect();
        if lines.len() > 1 {
            let indented_lines = lines.iter().filter(|l| l.starts_with("    ") || l.starts_with('\t')).count();
            if indented_lines > 0 {
                score += 0.2;
            }
        }

        // Reasonable length (not too short, not too long)
        let line_count = code.lines().count();
        if (3..=100).contains(&line_count) {
            score += 0.2;
        }

        // No obvious syntax errors (unclosed parens, brackets)
        let open_parens = code.matches('(').count();
        let close_parens = code.matches(')').count();
        let open_brackets = code.matches('[').count();
        let close_brackets = code.matches(']').count();

        if open_parens == close_parens && open_brackets == close_brackets {
            score += 0.2;
        }

        score
    }

    /// Estimate test pass rate based on code structure (simulated).
    ///
    /// In production, this would run the test_code in a sandbox with the model's solution.
    /// Here we use heuristics to simulate test pass rates.
    fn estimate_test_pass_rate(code: &str, task: &CodingTask) -> f64 {
        if code.trim().is_empty() {
            return 0.0;
        }

        let mut score: f64 = 0.0;

        // Extract the function name from the task
        let func_re = regex::Regex::new(r"function `(\w+)`").unwrap();
        if let Some(caps) = func_re.captures(&task.task) {
            let func_name = &caps[1];
            // Check if the code defines the expected function
            let def_re = format!(r"^\s*def {}\s*\(", regex::escape(func_name));
            if regex::Regex::new(&def_re).unwrap().is_match(code) {
                score += 0.5;
            }
        }

        // Has return statements
        if code.contains("return ") {
            score += 0.2;
        }

        // Has some logic (not just a stub)
        let logic_keywords = ["if ", "for ", "while ", "try:", "except", "assert ", "len(", "range(", "+", "-", "*", "/"];
        let keyword_count = logic_keywords.iter().filter(|&&kw| code.contains(kw)).count();
        if keyword_count >= 2 {
            score += 0.3;
        }

        // Penalize for TODO/placeholder comments
        if code.contains("TODO") || code.contains("pass\n") && code.lines().count() < 5 {
            score -= 0.2;
        }

        score.clamp(0.0, 1.0)
    }

    /// Compute reward breakdown.
    fn compute_reward_breakdown(
        &self,
        task: &CodingTask,
        result: &AgentResult,
    ) -> (f64, HashMap<String, f64>) {
        let code = Self::extract_python_code(&result.final_response);

        // Signal 1: Estimated test pass rate
        let test_pass = Self::estimate_test_pass_rate(&code, task);

        // Signal 2: Code quality
        let code_quality = Self::code_quality_score(&code);

        // Signal 3: LLM judge (simulated via keyword matching)
        // In production, this would call an LLM to judge the code quality
        let llm_judge = heuristic_score(&task.task, &result.final_response);

        // Composite reward
        let total_weight = self.config.test_pass_weight
            + self.config.code_quality_weight
            + self.config.llm_judge_weight;

        let reward = (self.config.test_pass_weight * test_pass
            + self.config.code_quality_weight * code_quality
            + self.config.llm_judge_weight * llm_judge)
            / total_weight.max(f64::EPSILON);

        let mut signals = HashMap::new();
        signals.insert("test_pass_rate".into(), test_pass);
        signals.insert("code_quality".into(), code_quality);
        signals.insert("llm_judge".into(), llm_judge);

        (reward.clamp(0.0, 1.0), signals)
    }
}

#[async_trait::async_trait]
impl Environment for AtroposEnv {
    fn name(&self) -> &str {
        "atropos"
    }

    async fn setup(&mut self) -> Result<(), EnvError> {
        let tasks = builtin_coding_tasks();
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
            "AtroposEnv setup: {} train / {} eval items",
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
        item.insert("task".into(), serde_json::Value::String(task.task.clone()));
        item.insert("test_code".into(), serde_json::Value::String(task.test_code.clone()));
        item.insert("difficulty".into(), serde_json::Value::String(task.difficulty.clone()));
        Ok(item)
    }

    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String {
        let task = match item.get("task") {
            Some(v) => v.as_str().unwrap_or("Unknown task"),
            None => "Unknown task",
        };
        format!(
            "Write a Python function to solve the following task.\n\n\
             Task: {}\n\n\
             Write only the function implementation. Make sure it passes the tests.",
            task
        )
    }

    async fn compute_reward(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> f64 {
        let task = self.find_task_for_item(item);
        let (reward, _signals) = match task {
            Some(t) => self.compute_reward_breakdown(&t, result),
            None => (0.0, HashMap::new()),
        };
        reward
    }

    async fn evaluate(&mut self) -> Result<HashMap<String, f64>, EnvError> {
        if self.eval_tasks.is_empty() {
            return Ok(HashMap::new());
        }

        let eval_size = self.config.eval_size.min(self.eval_tasks.len());
        let mut samples = Vec::new();
        let mut total_reward = 0.0;
        let mut total_test_pass = 0.0;

        for task in &self.eval_tasks[..eval_size] {
            let mut item = HashMap::new();
            item.insert("task".into(), serde_json::Value::String(task.task.clone()));
            item.insert("test_code".into(), serde_json::Value::String(task.test_code.clone()));

            let prompt = self.format_prompt(&item);
            let mut messages = Vec::new();
            if let Some(ref sys) = self.config.base.system_prompt {
                messages.push(Message::system(sys));
            }
            messages.push(Message::user(&prompt));

            let result = AgentResult::new(messages);
            let (reward, signals) = self.compute_reward_breakdown(task, &result);

            let test_pass = signals.get("test_pass_rate").copied().unwrap_or(0.0);
            total_test_pass += test_pass;
            total_reward += reward;

            samples.push(EvalSample {
                prompt: task.task.clone(),
                response: result.final_response.clone(),
                expected: task.test_code.clone(),
                correctness: test_pass,
                reward,
            });
        }

        let n = samples.len() as f64;
        let mut metrics = HashMap::new();
        metrics.insert("eval/mean_reward".into(), total_reward / n.max(f64::EPSILON));
        metrics.insert(
            "eval/mean_test_pass".into(),
            total_test_pass / n.max(f64::EPSILON),
        );
        metrics.insert("eval/n_items".into(), n);

        tracing::info!(
            "AtroposEnv eval: mean_reward={:.3}, mean_test_pass={:.3}",
            total_reward / n.max(f64::EPSILON),
            total_test_pass / n.max(f64::EPSILON)
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

        let task = self.find_task_for_item(item);
        let (reward, signals) = match task {
            Some(t) => self.compute_reward_breakdown(&t, &result),
            None => (0.0, HashMap::new()),
        };

        Ok(ScoredTrajectory {
            item: item.clone(),
            result,
            reward,
            reward_signals: Some(signals),
        })
    }
}

impl AtroposEnv {
    /// Find the coding task that matches the given item.
    fn find_task_for_item(&self, item: &HashMap<String, serde_json::Value>) -> Option<CodingTask> {
        let task_text = item.get("task").and_then(|v| v.as_str()).unwrap_or("");
        self.tasks
            .iter()
            .chain(self.eval_tasks.iter())
            .find(|t| t.task == task_text)
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_python_code_from_block() {
        let response = "Here's the solution:\n```python\ndef foo():\n    return 42\n```\n";
        let code = AtroposEnv::extract_python_code(response);
        assert!(code.contains("def foo()"));
    }

    #[test]
    fn test_extract_python_code_fallback() {
        let response = "def foo():\n    return 42\n";
        let code = AtroposEnv::extract_python_code(response);
        assert!(code.contains("def foo()"));
    }

    #[test]
    fn test_code_quality_non_empty() {
        let code = "def foo(x):\n    if x > 0:\n        return x\n    return 0\n";
        assert!(AtroposEnv::code_quality_score(code) > 0.0);
    }

    #[test]
    fn test_code_quality_empty() {
        let code = "";
        assert_eq!(AtroposEnv::code_quality_score(code), 0.0);
    }

    #[test]
    fn test_builtin_tasks_not_empty() {
        let tasks = builtin_coding_tasks();
        assert!(!tasks.is_empty());
        assert!(tasks.iter().any(|t| t.difficulty == "hard"));
    }

    #[tokio::test]
    async fn test_atropos_env_setup_and_next_item() {
        let mut env = AtroposEnv::new();
        env.setup().await.unwrap();
        let item = env.get_next_item().await.unwrap();
        assert!(item.contains_key("task"));
        assert!(item.contains_key("test_code"));
    }

    #[tokio::test]
    async fn test_atropos_env_format_prompt() {
        let env = AtroposEnv::new();
        let mut item = HashMap::new();
        item.insert("task".into(), serde_json::Value::String("Write foo()".into()));
        let prompt = env.format_prompt(&item);
        assert!(prompt.contains("Write foo()"));
    }

    #[tokio::test]
    async fn test_atropos_env_reward_no_code() {
        let env = AtroposEnv::new();
        let mut item = HashMap::new();
        item.insert(
            "task".into(),
            serde_json::Value::String("Write fizzbuzz".into()),
        );

        let messages = vec![
            Message::user("Write fizzbuzz"),
            Message::assistant("I don't know how."),
        ];
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward < 0.3);
    }

    #[tokio::test]
    async fn test_atropos_env_reward_with_code() {
        let mut env = AtroposEnv::new();
        env.setup().await.unwrap();

        // Find an actual task from the environment
        let task = env.tasks.first().cloned()
            .or_else(|| env.eval_tasks.first().cloned())
            .expect("environment should have tasks after setup");

        let mut item = HashMap::new();
        item.insert(
            "task".into(),
            serde_json::Value::String(task.task.clone()),
        );

        let code = r#"```python
def fizzbuzz(n):
    result = []
    for i in range(1, n + 1):
        if i % 15 == 0:
            result.append('FizzBuzz')
        elif i % 3 == 0:
            result.append('Fizz')
        elif i % 5 == 0:
            result.append('Buzz')
        else:
            result.append(str(i))
    return result
```"#;

        let messages = vec![
            Message::user(&task.task),
            Message::assistant(code),
        ];
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward > 0.15);
    }
}
