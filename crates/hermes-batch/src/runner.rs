//! Batch runner — parallel execution over a JSONL dataset.
//!
//! Mirrors the Python `BatchRunner` class in `batch_runner.py`.
//! Features:
//! - JSONL dataset loading with optional truncation
//! - Content-based resume from checkpoint
//! - Parallel batch execution via tokio::JoinSet
//! - Per-batch JSONL output writing
//! - Tool and reasoning statistics aggregation

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};

use hermes_core::{HermesError, Result};
use hermes_tools::registry::ToolRegistry;
use hermes_tools::register_all_tools;

use crate::checkpoint::{BatchStat, Checkpoint};
use crate::trajectories::{extract_reasoning_stats, extract_tool_stats, TrajectoryEntry, TrajectoryMessage};

/// Configuration for a batch run.
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Path to the JSONL dataset.
    pub dataset_file: String,
    /// Number of prompts per batch.
    pub batch_size: usize,
    /// Name for this run (used for output dir and checkpointing).
    pub run_name: String,
    /// Max tool-calling iterations per agent run.
    pub max_iterations: usize,
    /// Number of parallel workers (batches processed concurrently).
    pub num_workers: usize,
    /// Model to use.
    pub model: String,
    /// Base URL for the LLM API.
    pub base_url: Option<String>,
    /// API key for the LLM API.
    pub api_key: Option<String>,
    /// Truncate dataset to this many samples (0 = all).
    pub max_samples: usize,
    /// Output directory (defaults to `data/{run_name}`).
    pub output_dir: Option<String>,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            dataset_file: String::new(),
            batch_size: 10,
            run_name: String::from("default"),
            max_iterations: 90,
            num_workers: 4,
            model: String::from("anthropic/claude-opus-4.6"),
            base_url: None,
            api_key: None,
            max_samples: 0,
            output_dir: None,
        }
    }
}

/// A single prompt entry from a JSONL dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptEntry {
    /// The prompt text.
    pub prompt: String,
    /// Optional task ID.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Optional metadata.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Result of processing a single prompt.
#[derive(Debug, Clone, Serialize)]
pub struct PromptResult {
    pub success: bool,
    pub prompt_index: usize,
    pub trajectory: TrajectoryEntry,
    pub tool_stats: serde_json::Value,
    pub reasoning_stats: serde_json::Value,
    pub completed: bool,
    pub api_calls: usize,
}

/// Result of processing a single batch.
#[derive(Debug, Clone, Serialize)]
pub struct BatchResult {
    pub batch_num: usize,
    pub processed: usize,
    pub skipped: usize,
    pub tool_stats: serde_json::Value,
    pub reasoning_stats: serde_json::Value,
    pub completed_prompts: Vec<String>,
}

/// Main batch runner.
pub struct BatchRunner {
    config: BatchConfig,
    entries: Vec<PromptEntry>,
    checkpoint: Checkpoint,
    checkpoint_path: std::path::PathBuf,
    output_dir: std::path::PathBuf,
}

impl BatchRunner {
    /// Create a new batch runner from config.
    pub fn new(config: BatchConfig) -> Result<Self> {
        let output_dir = config
            .output_dir
            .clone()
            .unwrap_or_else(|| format!("data/{}", config.run_name));

        let output_path = std::path::Path::new(&output_dir);
        std::fs::create_dir_all(output_path).map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to create output directory {output_dir}: {e}"),
            )
        })?;

        let checkpoint_path = output_path.join("checkpoint.json");
        let checkpoint = Checkpoint::load(&checkpoint_path)
            .ok()
            .flatten()
            .unwrap_or_else(|| Checkpoint::new(&config.run_name));

        let entries = load_dataset(&config.dataset_file, config.max_samples)?;

        tracing::info!(
            "BatchRunner initialized: {} entries, {} batches (size={}), output={}",
            entries.len(),
            entries.len().div_ceil(config.batch_size),
            config.batch_size,
            output_dir
        );

        Ok(Self {
            config,
            entries,
            checkpoint,
            checkpoint_path,
            output_dir: output_path.to_path_buf(),
        })
    }

    /// Run all batches, optionally resuming from checkpoint.
    pub async fn run(&mut self, resume: bool) -> Result<RunSummary> {
        let total_entries = self.entries.len();
        let completed_set: std::collections::HashSet<String> =
            if resume {
                self.checkpoint.completed_prompts.iter().cloned().collect()
            } else {
                std::collections::HashSet::new()
            };

        // Filter out completed entries
        let pending: Vec<(usize, &PromptEntry)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !completed_set.contains(&entry.prompt))
            .collect();

        if pending.is_empty() {
            tracing::info!("All {} entries already completed (resume mode).", total_entries);
            return Ok(self.build_summary(total_entries, completed_set.len()));
        }

        tracing::info!(
            "Running {} entries ({} already completed)",
            pending.len(),
            completed_set.len()
        );

        // Create batches from pending entries
        let batches: Vec<(usize, Vec<(usize, &PromptEntry)>)> = pending
            .chunks(self.config.batch_size)
            .enumerate()
            .map(|(i, chunk)| (i, chunk.to_vec()))
            .collect();

        let total_batches = batches.len();
        let mut completed_prompts: std::collections::HashSet<String> = completed_set;
        let mut batch_results: Vec<BatchResult> = Vec::new();

        // Progress bar for batch processing
        let pb = ProgressBar::new(total_batches as u64);
        pb.set_style(ProgressStyle::with_template(
            "{spinner:.cyan/blue} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta}) {msg}",
        )
        .unwrap()
        .progress_chars("█░"));

        // Process batches with limited concurrency
        let concurrency = self.config.num_workers.min(batches.len());
        let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
        let registry = Arc::new({
            let mut r = ToolRegistry::new();
            register_all_tools(&mut r);
            r
        });

        // Shared progress counter for concurrent batches
        let completed_count = AtomicUsize::new(0);
        let failed_count = AtomicUsize::new(0);

        for (batch_num, batch_entries) in batches {
            let config = self.config.clone();
            let output_dir = self.output_dir.clone();
            let registry = Arc::clone(&registry);
            let mut completed = completed_prompts.clone();
            let pb_clone = pb.clone();

            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| HermesError::new(hermes_core::errors::ErrorCategory::InternalError, e.to_string()))?;

            // Process batch sequentially within the worker
            let result = process_batch(batch_num, &batch_entries, &config, &output_dir, &mut completed, &registry).await;

            drop(permit);

            match result {
                Ok(batch_result) => {
                    let processed = batch_result.processed;
                    let skipped = batch_result.skipped;

                    // Update checkpoint
                    let stat = BatchStat {
                        batch_num,
                        processed,
                        skipped,
                        total_api_calls: 0,
                        total_tokens_input: 0,
                        total_tokens_output: 0,
                    };
                    let _ = self.checkpoint.record_batch(stat, &self.checkpoint_path);
                    for prompt in &batch_result.completed_prompts {
                        let _ = self.checkpoint.record_completion(prompt, &self.checkpoint_path);
                    }

                    completed_count.fetch_add(processed, Ordering::Relaxed);
                    completed_prompts.extend(batch_result.completed_prompts.iter().cloned());
                    batch_results.push(batch_result);

                    pb_clone.inc(1);
                    pb_clone.set_message(format!(
                        "✓{}/✗{} this batch: {processed}p/{skipped}s",
                        completed_count.load(Ordering::Relaxed),
                        failed_count.load(Ordering::Relaxed)
                    ));
                }
                Err(e) => {
                    failed_count.fetch_add(1, Ordering::Relaxed);
                    tracing::error!("Batch {} failed: {}", batch_num, e);
                    pb_clone.inc(1);
                    pb_clone.set_message(format!(
                        "✗ batch {batch_num}: {e}"
                    ));
                }
            }
        }

        pb.finish_with_message("Batch run complete");

        // Combine batch files into trajectories
        let total_trajectories = crate::trajectories::combine_batch_files(&self.output_dir)
            .unwrap_or(0);

        tracing::info!(
            "Batch run complete: {} trajectories written",
            total_trajectories
        );

        Ok(self.build_summary(total_entries, completed_prompts.len()))
    }

    fn build_summary(&self, total: usize, completed: usize) -> RunSummary {
        RunSummary {
            total_entries: total,
            completed_entries: completed,
            total_batches: self.checkpoint.batch_stats.len(),
            output_dir: self.output_dir.to_string_lossy().to_string(),
        }
    }

    /// Get the output directory path.
    pub fn output_dir(&self) -> &Path {
        &self.output_dir
    }
}

/// Summary of a completed batch run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub total_entries: usize,
    pub completed_entries: usize,
    pub total_batches: usize,
    pub output_dir: String,
}

/// Process a single batch of prompts.
async fn process_batch(
    batch_num: usize,
    entries: &[(usize, &PromptEntry)],
    config: &BatchConfig,
    output_dir: &Path,
    completed_prompts: &mut std::collections::HashSet<String>,
    registry: &Arc<ToolRegistry>,
) -> Result<BatchResult> {
    let batch_file = output_dir.join(format!("batch_{batch_num}.jsonl"));
    let mut writer = std::io::BufWriter::new(
        std::fs::File::create(&batch_file).map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to create batch file: {e}"),
            )
        })?,
    );

    let mut processed = 0;
    let mut skipped = 0;
    let mut total_tool_stats = serde_json::Map::new();
    let mut total_reasoning_stats = serde_json::Map::new();
    let mut completed_prompts_list = Vec::new();

    for (prompt_index, entry) in entries {
        if completed_prompts.contains(&entry.prompt) {
            skipped += 1;
            continue;
        }

        // Create and run agent
        let agent_config = hermes_agent_engine::agent::AgentConfig {
            model: config.model.clone(),
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            max_iterations: config.max_iterations,
            skip_context_files: true,
            ..hermes_agent_engine::agent::AgentConfig::default()
        };

        let mut agent = hermes_agent_engine::AIAgent::new(agent_config, Arc::clone(registry))
            .map_err(|e| {
                HermesError::new(
                    hermes_core::errors::ErrorCategory::InternalError,
                    format!("Failed to create agent: {e}"),
                )
            })?;

        let turn_result = agent.run_conversation(&entry.prompt, None, None).await;

        // Convert messages to trajectory format
        let conversations: Vec<TrajectoryMessage> = turn_result
            .messages
            .iter()
            .filter_map(|msg| {
                let from = msg.get("role").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                let value = msg.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if from == "system" {
                    None // Skip system messages in trajectory
                } else {
                    Some(TrajectoryMessage { from, value })
                }
            })
            .collect();

        let tool_stats = extract_tool_stats(&turn_result.messages);
        let reasoning_stats = extract_reasoning_stats(&turn_result.messages);

        let trajectory = TrajectoryEntry {
            conversations,
            timestamp: chrono::Utc::now().to_rfc3339(),
            model: config.model.clone(),
            completed: turn_result.exit_reason == hermes_agent_engine::agent::ExitReason::Completed,
            tool_stats: Some(serde_json::to_value(&tool_stats).unwrap_or_default()),
            reasoning_stats: Some(serde_json::to_value(&reasoning_stats).unwrap_or_default()),
            metadata: Some(serde_json::json!({
                "prompt_index": prompt_index,
                "batch_num": batch_num,
                "exit_reason": turn_result.exit_reason,
            })),
        };

        // Write trajectory entry
        let line = serde_json::to_string(&trajectory)?;
        writeln!(writer, "{line}")?;

        processed += 1;
        completed_prompts_list.push(entry.prompt.clone());

        // Aggregate tool stats
        for (tool_name, value) in tool_stats.per_tool {
            let entry = total_tool_stats
                .entry(tool_name.clone())
                .or_insert_with(|| serde_json::json!({"calls": 0, "success": 0, "errors": 0}));
            if let Some(obj) = entry.as_object_mut() {
                let calls = obj.get("calls").and_then(|v| v.as_u64()).unwrap_or(0) as usize
                    + value.get("calls").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let success = obj.get("success").and_then(|v| v.as_u64()).unwrap_or(0) as usize
                    + value.get("success").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let errors = obj.get("errors").and_then(|v| v.as_u64()).unwrap_or(0) as usize
                    + value.get("errors").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                obj["calls"] = serde_json::json!(calls);
                obj["success"] = serde_json::json!(success);
                obj["errors"] = serde_json::json!(errors);
            }
        }

        // Aggregate reasoning stats
        let with_r = reasoning_stats.with_reasoning as u64
            + total_reasoning_stats.get("with_reasoning").and_then(|v| v.as_u64()).unwrap_or(0);
        let without_r = reasoning_stats.without_reasoning as u64
            + total_reasoning_stats.get("without_reasoning").and_then(|v| v.as_u64()).unwrap_or(0);
        let total_t = reasoning_stats.total_turns as u64
            + total_reasoning_stats.get("total_turns").and_then(|v| v.as_u64()).unwrap_or(0);
        total_reasoning_stats.insert("with_reasoning".to_string(), serde_json::json!(with_r));
        total_reasoning_stats.insert("without_reasoning".to_string(), serde_json::json!(without_r));
        total_reasoning_stats.insert("total_turns".to_string(), serde_json::json!(total_t));
    }

    writer.flush()?;

    Ok(BatchResult {
        batch_num,
        processed,
        skipped,
        tool_stats: serde_json::Value::Object(total_tool_stats),
        reasoning_stats: serde_json::Value::Object(total_reasoning_stats),
        completed_prompts: completed_prompts_list,
    })
}

/// Load a JSONL dataset file.
fn load_dataset(path: &str, max_samples: usize) -> Result<Vec<PromptEntry>> {
    let file = std::fs::File::open(path).map_err(|e| {
        HermesError::new(
            hermes_core::errors::ErrorCategory::InternalError,
            format!("Failed to open dataset file {path}: {e}"),
        )
    })?;

    let reader = std::io::BufReader::new(file);
    let mut entries = Vec::new();

    for (i, line) in std::io::BufRead::lines(reader).enumerate() {
        let line = line.map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to read line {i} of {path}: {e}"),
            )
        })?;

        if line.trim().is_empty() {
            continue;
        }

        let entry: PromptEntry = serde_json::from_str(&line).map_err(|e| {
            HermesError::new(
                hermes_core::errors::ErrorCategory::InternalError,
                format!("Failed to parse line {i} of {path}: {e}"),
            )
        })?;

        entries.push(entry);

        if max_samples > 0 && entries.len() >= max_samples {
            break;
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_dataset() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_dataset.jsonl");

        std::fs::write(
            &path,
            r#"{"prompt": "Hello world", "task_id": "t1"}
{"prompt": "Second prompt", "task_id": "t2"}
{"prompt": "Third prompt"}
"#,
        )
        .unwrap();

        let entries = load_dataset(path.to_str().unwrap(), 0).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].prompt, "Hello world");
        assert_eq!(entries[0].task_id, Some("t1".to_string()));
        assert_eq!(entries[2].task_id, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_dataset_truncated() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_dataset_trunc.jsonl");

        std::fs::write(
            &path,
            r#"{"prompt": "One"}
{"prompt": "Two"}
{"prompt": "Three"}
"#,
        )
        .unwrap();

        let entries = load_dataset(path.to_str().unwrap(), 2).unwrap();
        assert_eq!(entries.len(), 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_batch_config_default() {
        let config = BatchConfig::default();
        assert_eq!(config.batch_size, 10);
        assert_eq!(config.max_iterations, 90);
        assert_eq!(config.num_workers, 4);
    }

    #[test]
    fn test_batch_runner_new_creates_output_dir() {
        let dir = std::env::temp_dir().join("test_batch_runner_dir");
        let dataset = dir.join("dataset.jsonl");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(&dataset, r#"{"prompt": "test prompt"}"#).unwrap();

        let output_dir = dir.join("output");
        let config = BatchConfig {
            dataset_file: dataset.to_str().unwrap().to_string(),
            run_name: "test_run".to_string(),
            output_dir: Some(output_dir.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let runner = BatchRunner::new(config).unwrap();
        assert!(output_dir.exists());
        assert_eq!(runner.output_dir(), output_dir.as_path());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batch_runner_new_creates_checkpoint() {
        let dir = std::env::temp_dir().join("test_batch_checkpoint");
        let dataset = dir.join("dataset.jsonl");
        let _ = std::fs::create_dir_all(&dir);

        std::fs::write(&dataset, r#"{"prompt": "test"}"#).unwrap();

        let output_dir = dir.join("output");
        let config = BatchConfig {
            dataset_file: dataset.to_str().unwrap().to_string(),
            run_name: "cp_run".to_string(),
            output_dir: Some(output_dir.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let runner = BatchRunner::new(config).unwrap();
        let checkpoint_path = output_dir.join("checkpoint.json");
        // Checkpoint file is created on demand during save, not during new()
        // But the path should be set correctly
        assert_eq!(runner.checkpoint_path, checkpoint_path);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batch_runner_resume_uses_existing_checkpoint() {
        let dir = std::env::temp_dir().join("test_batch_resume");
        let dataset = dir.join("dataset.jsonl");
        let output_dir = dir.join("output");
        let _ = std::fs::create_dir_all(&output_dir);

        std::fs::write(&dataset, r#"{"prompt": "already done"}"#).unwrap();

        // Create a checkpoint with the prompt already completed
        let checkpoint = Checkpoint {
            run_name: "resume_run".to_string(),
            completed_prompts: vec!["already done".to_string()],
            batch_stats: vec![BatchStat {
                batch_num: 0,
                processed: 1,
                skipped: 0,
                total_api_calls: 0,
                total_tokens_input: 0,
                total_tokens_output: 0,
            }],
            last_updated: chrono::Utc::now().timestamp() as u64,
        };
        checkpoint.save(&output_dir.join("checkpoint.json")).unwrap();

        let config = BatchConfig {
            dataset_file: dataset.to_str().unwrap().to_string(),
            run_name: "resume_run".to_string(),
            output_dir: Some(output_dir.to_str().unwrap().to_string()),
            ..Default::default()
        };

        let runner = BatchRunner::new(config).unwrap();
        // Checkpoint should have been loaded
        assert!(runner.checkpoint.is_completed("already done"));
        assert_eq!(runner.checkpoint.batch_stats.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_batch_runner_new_fails_on_missing_dataset() {
        let config = BatchConfig {
            dataset_file: "/nonexistent/path/file.jsonl".to_string(),
            run_name: "fail_run".to_string(),
            ..Default::default()
        };

        let result = BatchRunner::new(config);
        assert!(result.is_err());
    }

    #[test]
    fn test_prompt_entry_extra_fields_preserved() {
        let json = r#"{"prompt": "hello", "task_id": "t1", "category": "math", "difficulty": 5}"#;
        let entry: PromptEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.prompt, "hello");
        assert_eq!(entry.task_id, Some("t1".to_string()));
        assert!(entry.extra.contains_key("category"));
        assert!(entry.extra.contains_key("difficulty"));
    }

    #[test]
    fn test_prompt_entry_minimal() {
        let json = r#"{"prompt": "just a prompt"}"#;
        let entry: PromptEntry = serde_json::from_str(json).unwrap();
        assert_eq!(entry.prompt, "just a prompt");
        assert!(entry.task_id.is_none());
        assert!(entry.extra.is_empty());
    }

    #[test]
    fn test_load_dataset_malformed_lines() {
        let dir = std::env::temp_dir();
        let path = dir.join("test_malformed.jsonl");

        std::fs::write(
            &path,
            r#"{"prompt": "valid line 1"}
not valid json at all
{"prompt": "valid line 2"}
"#,
        )
        .unwrap();

        let result = load_dataset(path.to_str().unwrap(), 0);
        // Should fail on the malformed line
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }
}
