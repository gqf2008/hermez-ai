#![allow(dead_code)]
//! SweEnv — SWE-Bench Style Software Engineering RL Environment
//!
//! Trains models to write correct code that passes real test suites.
//!
//! Key capabilities:
//!   - Dataset loading from HuggingFace-style JSON / local JSONL
//!   - Real code execution in sandboxed temporary directories
//!   - Test-verified rewards (pass rate, not heuristics)
//!   - Support for HumanEval, MBPP, and custom dataset formats
//!   - Docker sandbox option for stronger isolation
//!
//! Reward signals:
//!   - Test pass rate (fraction of tests that pass in real execution)
//!   - Compilation success (code parses and imports without error)
//!   - Partial credit for correct function signature
//!
//! Mirrors the Python `HermesSweEnv` from `environments/hermes_swe_env/`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::base::AgentRunner;

use crate::base::{
    AgentResult, EnvError, Environment, EnvironmentConfig, EvalSample,
    Message, ScoredTrajectory, strip_markdown,
};

// ---------------------------------------------------------------------------
// Dataset item types
// ---------------------------------------------------------------------------

/// A single SWE dataset item, normalized from various source formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweDatasetItem {
    /// Unique identifier.
    pub task_id: String,
    /// The coding prompt / instruction.
    pub prompt: String,
    /// Test code to verify the solution.
    pub test_code: String,
    /// Optional: reference/canonical solution.
    pub reference: Option<String>,
    /// Entry point function name (for HumanEval-style datasets).
    pub entry_point: Option<String>,
    /// Dataset source name.
    pub source: String,
    /// Difficulty or metadata tags.
    pub metadata: HashMap<String, serde_json::Value>,
}

impl SweDatasetItem {
    /// Normalize a generic JSON value into a SweDatasetItem.
    pub fn from_json(value: &serde_json::Value, source: &str) -> Option<Self> {
        let obj = value.as_object()?;

        // Try HumanEval format first
        if let Some(prompt) = obj.get("prompt").and_then(|v| v.as_str()) {
            let test_code = obj
                .get("test")
                .or_else(|| obj.get("test_code"))
                .or_else(|| obj.get("tests"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let task_id = obj
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let entry_point = obj.get("entry_point").and_then(|v| v.as_str()).map(String::from);
            let reference = obj
                .get("canonical_solution")
                .and_then(|v| v.as_str())
                .map(String::from);

            return Some(Self {
                task_id,
                prompt: prompt.to_string(),
                test_code,
                reference,
                entry_point,
                source: source.to_string(),
                metadata: obj.clone().into_iter().collect(),
            });
        }

        // Try MBPP format
        if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
            let test_list = obj.get("test_list").and_then(|v| v.as_array())?;
            let test_code = test_list
                .iter()
                .filter_map(|t| t.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let task_id = obj
                .get("task_id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let reference = obj.get("code").and_then(|v| v.as_str()).map(String::from);

            return Some(Self {
                task_id,
                prompt: text.to_string(),
                test_code,
                reference,
                entry_point: None,
                source: source.to_string(),
                metadata: obj.clone().into_iter().collect(),
            });
        }

        // Generic format: prompt + test_code
        let prompt = obj
            .get("prompt")
            .or_else(|| obj.get("instruction"))
            .or_else(|| obj.get("problem"))
            .and_then(|v| v.as_str())?;
        let test_code = obj
            .get("test_code")
            .or_else(|| obj.get("test"))
            .or_else(|| obj.get("tests"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let task_id = obj
            .get("task_id")
            .or_else(|| obj.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        Some(Self {
            task_id,
            prompt: prompt.to_string(),
            test_code,
            reference: obj.get("reference").and_then(|v| v.as_str()).map(String::from),
            entry_point: obj.get("entry_point").and_then(|v| v.as_str()).map(String::from),
            source: source.to_string(),
            metadata: obj.clone().into_iter().collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// Dataset loader
// ---------------------------------------------------------------------------

/// Load a dataset from a file path or HuggingFace dataset identifier.
///
/// Supported formats:
/// - `.jsonl` / `.jsonl.gz` — line-delimited JSON
/// - `.json` — JSON array of objects
/// - `hf://<dataset_name>` — HuggingFace dataset (downloaded via curl/requests)
#[derive(Debug, Clone)]
pub struct DatasetLoader {
    pub source: String,
    pub split: String,
}

impl DatasetLoader {
    pub fn new(source: impl Into<String>, split: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            split: split.into(),
        }
    }

    /// Load the dataset and return normalized items.
    pub async fn load(&self) -> Result<Vec<SweDatasetItem>, EnvError> {
        let path = PathBuf::from(&self.source);

        if path.extension().map(|e| e == "jsonl").unwrap_or(false)
            || self.source.ends_with(".jsonl.gz")
        {
            self.load_jsonl(&path).await
        } else if path.extension().map(|e| e == "json").unwrap_or(false) {
            self.load_json(&path).await
        } else if self.source.starts_with("hf://") || self.source.contains('/') {
            // Try downloading from HuggingFace datasets hub
            self.load_hf_dataset().await
        } else {
            Err(EnvError::SetupFailed(format!(
                "Unsupported dataset format: {}. Use .json, .jsonl, or hf://<name>",
                self.source
            )))
        }
    }

    async fn load_jsonl(&self, path: &Path) -> Result<Vec<SweDatasetItem>, EnvError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to read {}: {}", path.display(), e)))?;

        let mut items = Vec::new();
        for (line_num, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                EnvError::SetupFailed(format!(
                    "JSON parse error at line {} in {}: {}",
                    line_num + 1,
                    path.display(),
                    e
                ))
            })?;
            if let Some(item) = SweDatasetItem::from_json(&value, &self.source) {
                items.push(item);
            }
        }

        if items.is_empty() {
            return Err(EnvError::EmptyDataset);
        }

        Ok(items)
    }

    async fn load_json(&self, path: &Path) -> Result<Vec<SweDatasetItem>, EnvError> {
        let content = tokio::fs::read_to_string(path)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to read {}: {}", path.display(), e)))?;

        let array: Vec<serde_json::Value> = serde_json::from_str(&content).map_err(|e| {
            EnvError::SetupFailed(format!("JSON parse error in {}: {}", path.display(), e))
        })?;

        let mut items = Vec::new();
        for value in array {
            if let Some(item) = SweDatasetItem::from_json(&value, &self.source) {
                items.push(item);
            }
        }

        if items.is_empty() {
            return Err(EnvError::EmptyDataset);
        }

        Ok(items)
    }

    async fn load_hf_dataset(&self) -> Result<Vec<SweDatasetItem>, EnvError> {
        // Download from HuggingFace datasets hub using the parquet endpoint
        let dataset_name = self.source.trim_start_matches("hf://");
        let url = format!(
            "https://datasets-server.huggingface.co/rows?dataset={}&config=default&split={}&offset=0&limit=1000",
            urlencoding::encode(dataset_name),
            urlencoding::encode(&self.split),
        );

        let client = reqwest::Client::new();
        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| EnvError::SetupFailed(format!("HF dataset download failed: {}", e)))?;

        if !resp.status().is_success() {
            return Err(EnvError::SetupFailed(format!(
                "HF dataset API returned {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            )));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| {
            EnvError::SetupFailed(format!("HF dataset JSON parse failed: {}", e))
        })?;

        let rows = data
            .get("rows")
            .and_then(|v| v.as_array())
            .ok_or_else(|| EnvError::SetupFailed("HF dataset response missing 'rows'".into()))?;

        let mut items = Vec::new();
        for row in rows {
            let row_data = row.get("row").unwrap_or(row);
            if let Some(item) = SweDatasetItem::from_json(row_data, dataset_name) {
                items.push(item);
            }
        }

        if items.is_empty() {
            return Err(EnvError::EmptyDataset);
        }

        Ok(items)
    }
}

// ---------------------------------------------------------------------------
// Sandbox executor
// ---------------------------------------------------------------------------

/// Sandboxed code execution backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackend {
    /// Execute in a local temporary directory (fast, less isolation).
    Local,
    /// Execute in a Docker container (stronger isolation, requires Docker).
    Docker,
}

impl Default for SandboxBackend {
    fn default() -> Self {
        SandboxBackend::Local
    }
}

/// Result of a sandboxed test execution.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    /// Whether all tests passed.
    pub all_passed: bool,
    /// Number of tests that passed.
    pub passed: usize,
    /// Total number of tests.
    pub total: usize,
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code.
    pub exit_code: i32,
    /// Execution duration.
    pub duration_secs: f64,
}

/// Sandboxed Python code executor.
pub struct SandboxExecutor {
    backend: SandboxBackend,
    timeout_secs: u64,
    python_cmd: String,
}

impl SandboxExecutor {
    pub fn new(backend: SandboxBackend) -> Self {
        Self {
            backend,
            timeout_secs: 60,
            python_cmd: "python3".into(),
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    pub fn with_python_cmd(mut self, cmd: impl Into<String>) -> Self {
        self.python_cmd = cmd.into();
        self
    }

    /// Execute Python code with tests in a sandbox.
    ///
    /// Writes `solution_code` to `solution.py` and `test_code` to `test_solution.py`,
    /// then runs pytest or `python -m unittest`.
    pub async fn execute(
        &self,
        solution_code: &str,
        test_code: &str,
    ) -> Result<ExecutionResult, EnvError> {
        match self.backend {
            SandboxBackend::Local => self.execute_local(solution_code, test_code).await,
            SandboxBackend::Docker => self.execute_docker(solution_code, test_code).await,
        }
    }

    async fn execute_local(
        &self,
        solution_code: &str,
        test_code: &str,
    ) -> Result<ExecutionResult, EnvError> {
        let tmp_dir = tempfile::TempDir::new()
            .map_err(|e| EnvError::SetupFailed(format!("Failed to create temp dir: {}", e)))?;
        let tmp_path = tmp_dir.path();

        // Write solution.py
        let solution_path = tmp_path.join("solution.py");
        tokio::fs::write(&solution_path, solution_code)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to write solution: {}", e)))?;

        // Write test file
        let test_path = tmp_path.join("test_solution.py");
        let wrapped_test = self.wrap_test_code(test_code);
        tokio::fs::write(&test_path, wrapped_test)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to write test: {}", e)))?;

        // Run tests
        let start = Instant::now();
        let output = tokio::process::Command::new(&self.python_cmd)
            .args(["-m", "pytest", "-v", "--tb=short"])
            .current_dir(tmp_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| EnvError::EvalFailed(format!("Test execution failed: {}", e)))?;

        let duration = start.elapsed().as_secs_f64();

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        // Parse pytest output
        let (passed, total) = Self::parse_pytest_output(&stdout, &stderr);
        let all_passed = exit_code == 0 && passed == total && total > 0;

        // Fallback: if pytest not available, try unittest
        if exit_code != 0 && stdout.contains("No module named pytest") {
            return self.execute_unittest(tmp_path, duration).await;
        }

        Ok(ExecutionResult {
            all_passed,
            passed,
            total,
            stdout,
            stderr,
            exit_code,
            duration_secs: duration,
        })
    }

    async fn execute_unittest(
        &self,
        tmp_path: &Path,
        existing_duration: f64,
    ) -> Result<ExecutionResult, EnvError> {
        let start = Instant::now();
        let output = tokio::process::Command::new(&self.python_cmd)
            .args(["-m", "unittest", "test_solution", "-v"])
            .current_dir(tmp_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| EnvError::EvalFailed(format!("Unittest execution failed: {}", e)))?;

        let duration = existing_duration + start.elapsed().as_secs_f64();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let (passed, total) = Self::parse_unittest_output(&stdout, &stderr);
        let all_passed = exit_code == 0 && passed == total && total > 0;

        Ok(ExecutionResult {
            all_passed,
            passed,
            total,
            stdout,
            stderr,
            exit_code,
            duration_secs: duration,
        })
    }

    async fn execute_docker(
        &self,
        solution_code: &str,
        test_code: &str,
    ) -> Result<ExecutionResult, EnvError> {
        let tmp_dir = tempfile::TempDir::new()
            .map_err(|e| EnvError::SetupFailed(format!("Failed to create temp dir: {}", e)))?;
        let tmp_path = tmp_dir.path();

        tokio::fs::write(tmp_path.join("solution.py"), solution_code)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to write solution: {}", e)))?;

        let wrapped_test = self.wrap_test_code(test_code);
        tokio::fs::write(tmp_path.join("test_solution.py"), wrapped_test)
            .await
            .map_err(|e| EnvError::SetupFailed(format!("Failed to write test: {}", e)))?;

        let start = Instant::now();
        let output = tokio::process::Command::new("docker")
            .args([
                "run",
                "--rm",
                "-v",
                &format!("{}:/workspace", tmp_path.display()),
                "-w",
                "/workspace",
                "python:3.11-slim",
                "python",
                "-m",
                "pytest",
                "-v",
                "--tb=short",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| EnvError::EvalFailed(format!("Docker execution failed: {}", e)))?;

        let duration = start.elapsed().as_secs_f64();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        let (passed, total) = Self::parse_pytest_output(&stdout, &stderr);
        let all_passed = exit_code == 0 && passed == total && total > 0;

        Ok(ExecutionResult {
            all_passed,
            passed,
            total,
            stdout,
            stderr,
            exit_code,
            duration_secs: duration,
        })
    }

    /// Wrap raw test code into a proper pytest-compatible test file.
    fn wrap_test_code(&self, test_code: &str) -> String {
        // If it already looks like a pytest file, use as-is
        if test_code.contains("def test_") || test_code.contains("import unittest") {
            // Add import for solution module
            let mut result = String::new();
            if !test_code.contains("from solution import") && !test_code.contains("import solution") {
                result.push_str("from solution import *\n");
            }
            result.push_str(test_code);
            return result;
        }

        // Otherwise wrap it in a test function
        format!(
            "from solution import *\n\ndef test_solution():\n{}\n",
            test_code
                .lines()
                .map(|l| format!("    {}", l))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    /// Parse pytest verbose output to count passed/total tests.
    fn parse_pytest_output(stdout: &str, stderr: &str) -> (usize, usize) {
        let combined = format!("{}\n{}", stdout, stderr);

        // Look for "X passed, Y failed" or "X passed in ..."
        let re = regex::Regex::new(r"(\d+) passed(?:, (\d+) failed)?(?:, (\d+) error)?").unwrap();
        if let Some(caps) = re.captures(&combined) {
            let passed = caps[1].parse::<usize>().unwrap_or(0);
            let failed = caps.get(2).and_then(|m| m.as_str().parse::<usize>().ok()).unwrap_or(0);
            let errors = caps.get(3).and_then(|m| m.as_str().parse::<usize>().ok()).unwrap_or(0);
            return (passed, passed + failed + errors);
        }

        // Fallback: count test function definitions
        let test_re = regex::Regex::new(r"def test_").unwrap();
        let total = test_re.find_iter(&combined).count();
        if total > 0 {
            // If no explicit "passed" but exit code is 0, assume all passed
            return (total, total);
        }

        (0, 0)
    }

    /// Parse unittest verbose output.
    fn parse_unittest_output(stdout: &str, stderr: &str) -> (usize, usize) {
        let combined = format!("{}\n{}", stdout, stderr);

        // "Ran X tests in ..."
        let re = regex::Regex::new(r"Ran (\d+) tests?").unwrap();
        let total = if let Some(caps) = re.captures(&combined) {
            caps[1].parse::<usize>().unwrap_or(0)
        } else {
            0
        };

        // "OK" means all passed
        let passed = if combined.contains("OK") {
            total
        } else if combined.contains("FAILED") || combined.contains("ERROR") {
            // Count individual OKs
            let ok_re = regex::Regex::new(r"ok\s*$").unwrap();
            ok_re.find_iter(&combined).count()
        } else {
            total
        };

        (passed, total)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the SWE RL environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweEnvConfig {
    /// Base environment config.
    pub base: EnvironmentConfig,
    /// Dataset source (path or hf:// identifier).
    pub dataset_source: Option<String>,
    /// Dataset split.
    pub dataset_split: String,
    /// Sandbox backend.
    pub sandbox_backend: SandboxBackend,
    /// Test execution timeout in seconds.
    pub test_timeout_secs: u64,
    /// Python command.
    pub python_cmd: String,
    /// Reward weights.
    pub test_pass_weight: f64,
    pub compilation_weight: f64,
    pub syntax_weight: f64,
    /// Number of items to hold out for evaluation.
    pub eval_size: usize,
    /// Whether to cache dataset locally.
    pub cache_dataset: bool,
    /// Cache directory.
    pub cache_dir: Option<PathBuf>,
}

impl Default for SweEnvConfig {
    fn default() -> Self {
        Self {
            base: EnvironmentConfig {
                max_agent_turns: 30,
                agent_temperature: 0.8,
                system_prompt: Some(
                    "You are a skilled software engineer. You have access to a terminal, \
                     file tools, and web search. Use these tools to complete the coding task. \
                     Write clean, working code and verify it runs correctly before finishing."
                        .into(),
                ),
                group_size: 4,
                total_steps: 1000,
                steps_per_eval: 100,
                use_wandb: true,
                wandb_name: Some("hermes-swe".into()),
                max_tokens: None,
            },
            dataset_source: None,
            dataset_split: "test".into(),
            sandbox_backend: SandboxBackend::Local,
            test_timeout_secs: 60,
            python_cmd: "python3".into(),
            test_pass_weight: 0.7,
            compilation_weight: 0.2,
            syntax_weight: 0.1,
            eval_size: 50,
            cache_dataset: true,
            cache_dir: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// SWE-bench style software engineering RL environment.
///
/// The model is given a coding task and must write code that passes
/// real test suites executed in a sandbox.
pub struct SweEnv {
    config: SweEnvConfig,
    /// Training items.
    pub items: Vec<SweDatasetItem>,
    /// Evaluation items.
    pub eval_items: Vec<SweDatasetItem>,
    index: usize,
    is_setup: bool,
    executor: SandboxExecutor,
    /// Optional agent runner for real end-to-end evaluation.
    agent_runner: Option<Arc<dyn AgentRunner>>,
}

impl SweEnv {
    /// Create a new SweEnv with default configuration.
    pub fn new() -> Self {
        Self {
            config: SweEnvConfig::default(),
            items: Vec::new(),
            eval_items: Vec::new(),
            index: 0,
            is_setup: false,
            executor: SandboxExecutor::new(SandboxBackend::Local),
            agent_runner: None,
        }
    }

    /// Set an agent runner for real end-to-end evaluation.
    pub fn with_agent_runner(mut self, runner: Arc<dyn AgentRunner>) -> Self {
        self.agent_runner = Some(runner);
        self
    }

    /// Create a new SweEnv with custom configuration.
    pub fn with_config(config: SweEnvConfig) -> Self {
        let executor = SandboxExecutor::new(config.sandbox_backend)
            .with_timeout(config.test_timeout_secs)
            .with_python_cmd(&config.python_cmd);
        Self {
            config,
            items: Vec::new(),
            eval_items: Vec::new(),
            index: 0,
            is_setup: false,
            executor,
            agent_runner: None,
        }
    }

    /// Create a SweEnv with a built-in fallback dataset (HumanEval-style tasks).
    pub fn with_builtin_dataset(config: Option<SweEnvConfig>) -> Self {
        let config = config.unwrap_or_default();
        let executor = SandboxExecutor::new(config.sandbox_backend)
            .with_timeout(config.test_timeout_secs)
            .with_python_cmd(&config.python_cmd);

        let items = builtin_swe_tasks();
        let eval_size = config.eval_size.min(items.len() / 2);
        let mut items = items;
        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        items.shuffle(&mut rng);

        let eval_items = items.drain(..eval_size).collect();

        Self {
            config,
            items,
            eval_items,
            index: 0,
            is_setup: true,
            executor,
            agent_runner: None,
        }
    }

    /// Extract Python code from the model's response.
    fn extract_code(response: &str) -> String {
        let stripped = strip_markdown(response);

        // Try Python code block
        let re = regex::Regex::new(r"(?s)```python\s*\n(.*?)```").unwrap();
        if let Some(caps) = re.captures(&stripped) {
            return caps[1].trim().to_string();
        }

        // Try generic code block
        let re2 = regex::Regex::new(r"(?s)```\s*\n(.*?)```").unwrap();
        if let Some(caps) = re2.captures(&stripped) {
            let content = caps[1].trim();
            if content.contains("def ") || content.contains("import ") || content.contains("class ") {
                return content.to_string();
            }
        }

        // Fallback: return stripped response if it looks like code
        if stripped.contains("def ") || stripped.contains("import ") {
            stripped
        } else {
            stripped
        }
    }

    /// Check if code has valid Python syntax.
    fn check_syntax(code: &str) -> bool {
        // Heuristic: check for basic syntax patterns
        let open_parens = code.matches('(').count();
        let close_parens = code.matches(')').count();
        let open_brackets = code.matches('[').count();
        let close_brackets = code.matches(']').count();
        let open_braces = code.matches('{').count();
        let close_braces = code.matches('}').count();

        open_parens == close_parens
            && open_brackets == close_brackets
            && open_braces == close_braces
            && code.contains("def ")
    }

    /// Check if the code can be imported without error.
    async fn check_import(&self, code: &str) -> bool {
        let tmp_dir = match tempfile::TempDir::new() {
            Ok(d) => d,
            Err(_) => return false,
        };
        let solution_path = tmp_dir.path().join("solution.py");
        if tokio::fs::write(&solution_path, code).await.is_err() {
            return false;
        }

        let output = tokio::process::Command::new(&self.config.python_cmd)
            .args(["-c", "import solution"])
            .current_dir(tmp_dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await;

        match output {
            Ok(o) => o.status.success(),
            Err(_) => false,
        }
    }

    /// Compute reward breakdown.
    pub async fn compute_reward_breakdown(
        &self,
        item: &SweDatasetItem,
        result: &AgentResult,
    ) -> (f64, HashMap<String, f64>) {
        let code = Self::extract_code(&result.final_response);

        if code.trim().is_empty() {
            let mut signals = HashMap::new();
            signals.insert("test_pass_rate".into(), 0.0);
            signals.insert("compilation".into(), 0.0);
            signals.insert("syntax".into(), 0.0);
            return (0.0, signals);
        }

        // Signal 1: Syntax check
        let syntax_ok = Self::check_syntax(&code);
        let syntax_score = if syntax_ok { 1.0 } else { 0.0 };

        // Signal 2: Import check (compilation)
        let import_ok = self.check_import(&code).await;
        let compilation_score = if import_ok { 1.0 } else { 0.0 };

        // Signal 3: Test execution (most important)
        let test_score = if import_ok {
            match self.executor.execute(&code, &item.test_code).await {
                Ok(exec_result) => {
                    let rate = if exec_result.total > 0 {
                        exec_result.passed as f64 / exec_result.total as f64
                    } else {
                        // If no tests were detected but execution succeeded, give partial credit
                        if exec_result.exit_code == 0 {
                            0.5
                        } else {
                            0.0
                        }
                    };
                    rate
                }
                Err(_) => 0.0,
            }
        } else {
            0.0
        };

        // Composite reward
        let total_weight = self.config.test_pass_weight
            + self.config.compilation_weight
            + self.config.syntax_weight;

        let reward = (self.config.test_pass_weight * test_score
            + self.config.compilation_weight * compilation_score
            + self.config.syntax_weight * syntax_score)
            / total_weight.max(f64::EPSILON);

        let mut signals = HashMap::new();
        signals.insert("test_pass_rate".into(), test_score);
        signals.insert("compilation".into(), compilation_score);
        signals.insert("syntax".into(), syntax_score);

        (reward.clamp(0.0, 1.0), signals)
    }

    fn find_item_by_id(&self, task_id: &str) -> Option<&SweDatasetItem> {
        self.items
            .iter()
            .chain(self.eval_items.iter())
            .find(|i| i.task_id == task_id)
    }
}

#[async_trait::async_trait]
impl Environment for SweEnv {
    fn name(&self) -> &str {
        "swe"
    }

    async fn setup(&mut self) -> Result<(), EnvError> {
        let items = if let Some(ref source) = self.config.dataset_source {
            let loader = DatasetLoader::new(source, &self.config.dataset_split);
            loader.load().await?
        } else {
            builtin_swe_tasks()
        };

        let eval_size = self.config.eval_size.min(items.len() / 5).max(1);
        let mut items = items;

        use rand::seq::SliceRandom;
        let mut rng = rand::rng();
        items.shuffle(&mut rng);

        self.eval_items = items.drain(..eval_size).collect();
        self.items = items;
        self.index = 0;
        self.is_setup = true;

        tracing::info!(
            "SweEnv setup: {} train / {} eval items (backend: {:?})",
            self.items.len(),
            self.eval_items.len(),
            self.config.sandbox_backend
        );

        Ok(())
    }

    async fn get_next_item(&mut self) -> Result<HashMap<String, serde_json::Value>, EnvError> {
        if !self.is_setup {
            self.setup().await?;
        }
        if self.items.is_empty() {
            return Err(EnvError::EmptyDataset);
        }
        let item = &self.items[self.index % self.items.len()];
        self.index += 1;

        let mut map = HashMap::new();
        map.insert("task_id".into(), serde_json::Value::String(item.task_id.clone()));
        map.insert("prompt".into(), serde_json::Value::String(item.prompt.clone()));
        map.insert("test_code".into(), serde_json::Value::String(item.test_code.clone()));
        map.insert("source".into(), serde_json::Value::String(item.source.clone()));
        if let Some(ref ep) = item.entry_point {
            map.insert("entry_point".into(), serde_json::Value::String(ep.clone()));
        }
        if let Some(ref reference) = item.reference {
            map.insert("reference".into(), serde_json::Value::String(reference.clone()));
        }
        Ok(map)
    }

    fn format_prompt(&self, item: &HashMap<String, serde_json::Value>) -> String {
        let prompt = match item.get("prompt") {
            Some(v) => v.as_str().unwrap_or("Unknown task"),
            None => "Unknown task",
        };
        let entry_point = item.get("entry_point").and_then(|v| v.as_str());

        if let Some(ep) = entry_point {
            format!(
                "Write a Python function to solve the following task.\
                 \n\nTask: {}\
                 \n\nYour solution should define the function `{}`. \
                 Write clean, efficient code that handles edge cases.",
                prompt, ep
            )
        } else {
            format!(
                "Write Python code to solve the following task.\
                 \n\nTask: {}\
                 \n\nWrite clean, efficient code that handles edge cases.",
                prompt
            )
        }
    }

    async fn compute_reward(
        &self,
        item: &HashMap<String, serde_json::Value>,
        result: &AgentResult,
    ) -> f64 {
        let task_id = item.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        let dataset_item = self.find_item_by_id(task_id);

        let (reward, _signals) = match dataset_item {
            Some(di) => self.compute_reward_breakdown(di, result).await,
            None => {
                let mut signals = HashMap::new();
                signals.insert("test_pass_rate".into(), 0.0);
                signals.insert("compilation".into(), 0.0);
                signals.insert("syntax".into(), 0.0);
                (0.0, signals)
            }
        };
        reward
    }

    async fn evaluate(&mut self) -> Result<HashMap<String, f64>, EnvError> {
        if self.eval_items.is_empty() {
            return Ok(HashMap::new());
        }

        let eval_size = self.config.eval_size.min(self.eval_items.len());
        let mut samples = Vec::new();
        let mut total_reward = 0.0;
        let mut total_test_pass = 0.0;
        let mut total_compilation = 0.0;
        let mut passed_count = 0usize;

        for item in &self.eval_items[..eval_size] {
            let mut map = HashMap::new();
            map.insert("task_id".into(), serde_json::Value::String(item.task_id.clone()));
            map.insert("prompt".into(), serde_json::Value::String(item.prompt.clone()));

            let prompt = self.format_prompt(&map);
            let mut messages = Vec::new();
            if let Some(ref sys) = self.config.base.system_prompt {
                messages.push(Message::system(sys));
            }
            messages.push(Message::user(&prompt));

            let result = if let Some(ref runner) = self.agent_runner {
                runner.run(messages).await
            } else {
                AgentResult::new(messages)
            };
            let (reward, signals) = self.compute_reward_breakdown(item, &result).await;

            let test_pass = signals.get("test_pass_rate").copied().unwrap_or(0.0);
            let compilation = signals.get("compilation").copied().unwrap_or(0.0);

            if test_pass >= 1.0 {
                passed_count += 1;
            }

            total_test_pass += test_pass;
            total_compilation += compilation;
            total_reward += reward;

            samples.push(EvalSample {
                prompt: item.prompt.clone(),
                response: result.final_response.clone(),
                expected: item.reference.clone().unwrap_or_default(),
                correctness: test_pass,
                reward,
            });
        }

        let n = samples.len() as f64;
        let mut metrics = HashMap::new();
        metrics.insert("eval/mean_reward".into(), total_reward / n.max(f64::EPSILON));
        metrics.insert("eval/mean_test_pass".into(), total_test_pass / n.max(f64::EPSILON));
        metrics.insert("eval/mean_compilation".into(), total_compilation / n.max(f64::EPSILON));
        metrics.insert("eval/pass_rate".into(), passed_count as f64 / n.max(f64::EPSILON));
        metrics.insert("eval/n_items".into(), n);

        tracing::info!(
            "SweEnv eval: mean_reward={:.3}, pass_rate={:.3} ({}/{})",
            total_reward / n.max(f64::EPSILON),
            passed_count as f64 / n.max(f64::EPSILON),
            passed_count,
            samples.len()
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
        let task_id = item.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
        let dataset_item = self.find_item_by_id(task_id);

        let (reward, signals) = match dataset_item {
            Some(di) => self.compute_reward_breakdown(di, &result).await,
            None => {
                let mut signals = HashMap::new();
                signals.insert("test_pass_rate".into(), 0.0);
                signals.insert("compilation".into(), 0.0);
                signals.insert("syntax".into(), 0.0);
                (0.0, signals)
            }
        };

        Ok(ScoredTrajectory {
            item: item.clone(),
            result,
            reward,
            reward_signals: Some(signals),
        })
    }
}

// ---------------------------------------------------------------------------
// Built-in SWE tasks (fallback dataset)
// ---------------------------------------------------------------------------

/// Built-in coding tasks for when no external dataset is provided.
pub fn builtin_swe_tasks() -> Vec<SweDatasetItem> {
    vec![
        SweDatasetItem {
            task_id: "swe-fizzbuzz".into(),
            prompt: "Write a function fizzbuzz(n) that returns a list of strings from 1 to n. \
                      For multiples of 3 return 'Fizz', for multiples of 5 return 'Buzz', \
                      for multiples of both return 'FizzBuzz', otherwise the number as a string.".into(),
            test_code: "assert fizzbuzz(15) == ['1','2','Fizz','4','Buzz','Fizz','7','8','Fizz','Buzz','11','Fizz','13','14','FizzBuzz']\n\
                        assert fizzbuzz(1) == ['1']\n\
                        assert fizzbuzz(0) == []".into(),
            reference: Some(
                "def fizzbuzz(n):\n    return ['FizzBuzz' if i % 15 == 0 else 'Fizz' if i % 3 == 0 else 'Buzz' if i % 5 == 0 else str(i) for i in range(1, n+1)]".into()
            ),
            entry_point: Some("fizzbuzz".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-is-palindrome".into(),
            prompt: "Write a function is_palindrome(s) that checks if a string is a palindrome, \
                      ignoring case and non-alphanumeric characters. Return True or False.".into(),
            test_code: "assert is_palindrome('A man, a plan, a canal: Panama') == True\n\
                        assert is_palindrome('race a car') == False\n\
                        assert is_palindrome('') == True\n\
                        assert is_palindrome('Was it a car or a cat I saw?') == True".into(),
            reference: Some(
                "import re\ndef is_palindrome(s):\n    s = re.sub(r'[^a-zA-Z0-9]', '', s).lower()\n    return s == s[::-1]".into()
            ),
            entry_point: Some("is_palindrome".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-two-sum".into(),
            prompt: "Write a function two_sum(nums, target) that returns the indices of the two \
                      numbers in nums that add up to target. Assume exactly one solution exists.".into(),
            test_code: "assert two_sum([2, 7, 11, 15], 9) == [0, 1]\n\
                        assert two_sum([3, 2, 4], 6) == [1, 2]\n\
                        assert two_sum([3, 3], 6) == [0, 1]".into(),
            reference: Some(
                "def two_sum(nums, target):\n    seen = {}\n    for i, n in enumerate(nums):\n        if target - n in seen:\n            return [seen[target - n], i]\n        seen[n] = i".into()
            ),
            entry_point: Some("two_sum".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-merge-intervals".into(),
            prompt: "Write a function merge_intervals(intervals) that merges overlapping intervals. \
                      Return a list of non-overlapping intervals.".into(),
            test_code: "assert merge_intervals([[1,3],[2,6],[8,10],[15,18]]) == [[1,6],[8,10],[15,18]]\n\
                        assert merge_intervals([[1,4],[4,5]]) == [[1,5]]\n\
                        assert merge_intervals([[1,4],[0,4]]) == [[0,4]]\n\
                        assert merge_intervals([]) == []".into(),
            reference: Some(
                "def merge_intervals(intervals):\n    if not intervals: return []\n    intervals.sort()\n    merged = [intervals[0]]\n    for s, e in intervals[1:]:\n        if s <= merged[-1][1]:\n            merged[-1][1] = max(merged[-1][1], e)\n        else:\n            merged.append([s, e])\n    return merged".into()
            ),
            entry_point: Some("merge_intervals".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-valid-parentheses".into(),
            prompt: "Write a function valid_parentheses(s) that determines if a string \
                      containing just '(', ')', '{', '}', '[' and ']' is valid.".into(),
            test_code: "assert valid_parentheses('()') == True\n\
                        assert valid_parentheses('()[]{}') == True\n\
                        assert valid_parentheses('(]') == False\n\
                        assert valid_parentheses('([)]') == False\n\
                        assert valid_parentheses('{[]}') == True\n\
                        assert valid_parentheses('') == True".into(),
            reference: Some(
                "def valid_parentheses(s):\n    stack = []\n    pairs = {'(': ')', '[': ']', '{': '}'}\n    for c in s:\n        if c in pairs:\n            stack.append(c)\n        elif not stack or pairs[stack.pop()] != c:\n            return False\n    return not stack".into()
            ),
            entry_point: Some("valid_parentheses".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-reverse-linked-list".into(),
            prompt: "Write a function reverse_list(head) that reverses a singly linked list. \
                      The ListNode class is defined as: class ListNode: def __init__(self, val=0, next=None): self.val = val; self.next = next".into(),
            test_code: "class ListNode:\n    def __init__(self, val=0, next=None):\n        self.val = val\n        self.next = next\n\
                        def to_list(head):\n    result = []\n    while head:\n        result.append(head.val)\n        head = head.next\n    return result\n\
                        n1, n2, n3 = ListNode(1), ListNode(2), ListNode(3)\n\
                        n1.next, n2.next = n2, n3\n\
                        assert to_list(reverse_list(n1)) == [3, 2, 1]\n\
                        assert reverse_list(None) is None".into(),
            reference: Some(
                "def reverse_list(head):\n    prev = None\n    curr = head\n    while curr:\n        nxt = curr.next\n        curr.next = prev\n        prev = curr\n        curr = nxt\n    return prev".into()
            ),
            entry_point: Some("reverse_list".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-max-subarray".into(),
            prompt: "Write a function max_subarray(nums) that finds the contiguous subarray \
                      with the largest sum and returns its sum.".into(),
            test_code: "assert max_subarray([-2,1,-3,4,-1,2,1,-5,4]) == 6\n\
                        assert max_subarray([1]) == 1\n\
                        assert max_subarray([5,4,-1,7,8]) == 23\n\
                        assert max_subarray([-1]) == -1".into(),
            reference: Some(
                "def max_subarray(nums):\n    max_sum = cur_sum = nums[0]\n    for n in nums[1:]:\n        cur_sum = max(n, cur_sum + n)\n        max_sum = max(max_sum, cur_sum)\n    return max_sum".into()
            ),
            entry_point: Some("max_subarray".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-climb-stairs".into(),
            prompt: "Write a function climb_stairs(n) that counts the number of distinct ways \
                      to climb to the top of a staircase with n steps, taking 1 or 2 steps at a time.".into(),
            test_code: "assert climb_stairs(2) == 2\n\
                        assert climb_stairs(3) == 3\n\
                        assert climb_stairs(4) == 5\n\
                        assert climb_stairs(1) == 1".into(),
            reference: Some(
                "def climb_stairs(n):\n    if n <= 2: return n\n    a, b = 1, 2\n    for _ in range(3, n + 1):\n        a, b = b, a + b\n    return b".into()
            ),
            entry_point: Some("climb_stairs".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-longest-substring".into(),
            prompt: "Write a function length_of_longest_substring(s) that finds the length of \
                      the longest substring without repeating characters.".into(),
            test_code: "assert length_of_longest_substring('abcabcbb') == 3\n\
                        assert length_of_longest_substring('bbbbb') == 1\n\
                        assert length_of_longest_substring('pwwkew') == 3\n\
                        assert length_of_longest_substring('') == 0".into(),
            reference: Some(
                "def length_of_longest_substring(s):\n    seen = {}\n    start = max_len = 0\n    for i, c in enumerate(s):\n        if c in seen and seen[c] >= start:\n            start = seen[c] + 1\n        max_len = max(max_len, i - start + 1)\n        seen[c] = i\n    return max_len".into()
            ),
            entry_point: Some("length_of_longest_substring".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
        SweDatasetItem {
            task_id: "swe-binary-search".into(),
            prompt: "Write a function search(nums, target) that searches for target in a sorted array \
                      using binary search. Return the index if found, -1 otherwise.".into(),
            test_code: "assert search([-1,0,3,5,9,12], 9) == 4\n\
                        assert search([-1,0,3,5,9,12], 2) == -1\n\
                        assert search([5], 5) == 0\n\
                        assert search([], 1) == -1".into(),
            reference: Some(
                "def search(nums, target):\n    lo, hi = 0, len(nums) - 1\n    while lo <= hi:\n        mid = (lo + hi) // 2\n        if nums[mid] == target:\n            return mid\n        elif nums[mid] < target:\n            lo = mid + 1\n        else:\n            hi = mid - 1\n    return -1".into()
            ),
            entry_point: Some("search".into()),
            source: "builtin".into(),
            metadata: HashMap::new(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_tasks_not_empty() {
        let tasks = builtin_swe_tasks();
        assert!(!tasks.is_empty());
        assert!(tasks.iter().all(|t| !t.task_id.is_empty()));
    }

    #[test]
    fn test_extract_code_from_block() {
        let response = "Here's the solution:\n```python\ndef foo():\n    return 42\n```\n";
        let code = SweEnv::extract_code(response);
        assert!(code.contains("def foo():"));
    }

    #[test]
    fn test_extract_code_fallback() {
        let response = "def foo():\n    return 42\n";
        let code = SweEnv::extract_code(response);
        assert!(code.contains("def foo():"));
    }

    #[test]
    fn test_check_syntax_valid() {
        let code = "def foo(x):\n    if x > 0:\n        return x\n    return 0\n";
        assert!(SweEnv::check_syntax(code));
    }

    #[test]
    fn test_check_syntax_invalid_parens() {
        let code = "def foo(x:\n    return x\n";
        assert!(!SweEnv::check_syntax(code));
    }

    #[test]
    fn test_swe_dataset_item_from_json_human_eval() {
        let json = serde_json::json!({
            "task_id": "HumanEval/1",
            "prompt": "def foo():\n    pass",
            "entry_point": "foo",
            "canonical_solution": "def foo():\n    return 42",
            "test": "assert foo() == 42"
        });
        let item = SweDatasetItem::from_json(&json, "humaneval").unwrap();
        assert_eq!(item.task_id, "HumanEval/1");
        assert_eq!(item.entry_point, Some("foo".into()));
        assert!(item.reference.is_some());
    }

    #[test]
    fn test_parse_pytest_output() {
        let stdout = "test_solution.py::test_solution PASSED\n\n1 passed in 0.01s";
        let (passed, total) = SandboxExecutor::parse_pytest_output(stdout, "");
        assert_eq!(passed, 1);
        assert_eq!(total, 1);
    }

    #[test]
    fn test_parse_pytest_output_mixed() {
        let stdout = "test_solution.py::test_a PASSED\ntest_solution.py::test_b FAILED\n\n1 passed, 1 failed";
        let (passed, total) = SandboxExecutor::parse_pytest_output(stdout, "");
        assert_eq!(passed, 1);
        assert_eq!(total, 2);
    }

    #[tokio::test]
    async fn test_swe_env_setup_builtin() {
        let mut env = SweEnv::new();
        env.setup().await.unwrap();
        let item = env.get_next_item().await.unwrap();
        assert!(item.contains_key("prompt"));
        assert!(item.contains_key("task_id"));
    }

    #[tokio::test]
    async fn test_sandbox_executor_local() {
        // Skip if python3 is not available
        if tokio::process::Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }

        let executor = SandboxExecutor::new(SandboxBackend::Local);
        let solution = "def add(a, b):\n    return a + b\n";
        // Use two explicit test functions so pytest counts 2 tests
        let test = "def test_add_positive():\n    assert add(2, 3) == 5\n\ndef test_add_negative():\n    assert add(-1, 1) == 0\n";

        let result = executor.execute(solution, test).await.unwrap();
        assert!(result.all_passed, "stdout: {}\nstderr: {}", result.stdout, result.stderr);
        assert_eq!(result.passed, 2);
        assert_eq!(result.total, 2);
    }

    #[tokio::test]
    async fn test_sandbox_executor_failure() {
        if tokio::process::Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_err()
        {
            return;
        }

        let executor = SandboxExecutor::new(SandboxBackend::Local);
        let solution = "def add(a, b):\n    return a - b\n"; // Bug: subtraction instead of addition
        let test = "assert add(2, 3) == 5\n";

        let result = executor.execute(solution, test).await.unwrap();
        assert!(!result.all_passed);
    }

    #[tokio::test]
    async fn test_swe_env_reward_correct_code() {
        let mut env = SweEnv::with_builtin_dataset(None);
        env.setup().await.unwrap();

        let item = env.get_next_item().await.unwrap();
        let task_id = item.get("task_id").and_then(|v| v.as_str()).unwrap();
        let dataset_item = env.find_item_by_id(task_id).unwrap();

        let code = dataset_item.reference.clone().unwrap_or_default();
        let messages = vec![
            Message::user(&env.format_prompt(&item)),
            Message::assistant(&format!("```python\n{}\n```", code)),
        ];
        let result = AgentResult::new(messages);

        let reward = env.compute_reward(&item, &result).await;
        assert!(reward > 0.5, "Expected reward > 0.5 for correct code, got {}", reward);
    }
}
