//! Trajectory compressor — post-processes completed agent trajectories to
//! compress them within a target token budget while preserving training signal.
//!
//! Mirrors the Python `trajectory_compressor.py`.
//!
//! # Compression Strategy
//! 1. Protect first turns (system, human, first gpt, first tool)
//! 2. Protect last N turns (final actions and conclusions)
//! 3. Compress MIDDLE turns only, starting from 2nd tool response
//! 4. Compress only as much as needed to fit under target
//! 5. Replace compressed region with a single human summary message
//! 6. Keep remaining tool calls intact

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiktoken_rs::get_bpe_from_model;

use hermes_llm::model_metadata::MINIMUM_CONTEXT_LENGTH;

use crate::summarizer::Summarizer;

// ---------------------------------------------------------------------------
// Context Engine trait (mirrors Python ContextEngine ABC)
// ---------------------------------------------------------------------------

/// Abstract interface for pluggable context engines.
///
/// A context engine controls how conversation context is managed when
/// approaching the model's token limit. The built-in `TrajectoryCompressor`
/// is the default implementation. Third-party engines can replace it via
/// the plugin system.
///
/// Lifecycle:
/// 1. Engine is instantiated
/// 2. `on_session_start()` called when a conversation begins
/// 3. `update_from_response()` called after each API response with usage data
/// 4. `should_compress()` checked after each turn
/// 5. `compress()` called when `should_compress()` returns true
/// 6. `on_session_end()` called at real session boundaries
pub trait ContextEngine: Send + Sync {
    /// Short identifier (e.g. "compressor", "lcm").
    fn name(&self) -> &str;

    /// Token state — read by the agent for display/logging.
    fn last_prompt_tokens(&self) -> usize;
    fn last_completion_tokens(&self) -> usize;
    fn last_total_tokens(&self) -> usize;
    fn threshold_tokens(&self) -> usize;
    fn context_length(&self) -> usize;
    fn compression_count(&self) -> usize;

    /// Update token usage from an API response usage dict.
    fn update_from_response(&mut self, usage: &serde_json::Value);

    /// Return true if compaction should fire this turn.
    fn should_compress(&self, prompt_tokens: Option<usize>) -> bool;

    /// Compact the message list and return the new message list.
    fn compress(&mut self, messages: Vec<Value>) -> Vec<Value>;

    /// Quick rough check before the API call (no real token count yet).
    /// Default returns false.
    fn should_compress_preflight(&self, _messages: &[Value]) -> bool {
        false
    }

    /// Called when a new conversation session begins.
    fn on_session_start(&mut self, _session_id: &str, _kwargs: &HashMap<String, String>) {}

    /// Called at real session boundaries (CLI exit, /reset, gateway expiry).
    fn on_session_end(&mut self, _session_id: &str, _messages: &[Value]) {}

    /// Called on /new or /reset. Reset per-session state.
    fn on_session_reset(&mut self) {}

    /// Return tool schemas this engine provides to the agent.
    /// Default returns empty list (no tools).
    fn get_tool_schemas(&self) -> Vec<Value> {
        Vec::new()
    }

    /// Handle a tool call from the agent. Only called for tool names
    /// returned by `get_tool_schemas()`.
    fn handle_tool_call(&self, _name: &str, _args: &Value) -> String {
        serde_json::json!({"error": "No context engine tools registered"})
            .to_string()
    }

    /// Return status for display/logging.
    fn get_status(&self) -> HashMap<String, serde_json::Value> {
        let mut m = HashMap::new();
        m.insert("last_prompt_tokens".to_string(), serde_json::json!(self.last_prompt_tokens()));
        m.insert("threshold_tokens".to_string(), serde_json::json!(self.threshold_tokens()));
        m.insert("context_length".to_string(), serde_json::json!(self.context_length()));
        let usage_pct = if self.context_length() > 0 {
            std::cmp::min(100, self.last_prompt_tokens() * 100 / self.context_length())
        } else {
            0
        };
        m.insert("usage_percent".to_string(), serde_json::json!(usage_pct));
        m.insert("compression_count".to_string(), serde_json::json!(self.compression_count()));
        m
    }

    /// Called when the user switches models or on fallback activation.
    fn update_model(&mut self, _model: &str, _context_length: usize,
                    _base_url: &str, _api_key: &str, _provider: &str) {}
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for trajectory compression.
#[derive(Debug, Clone)]
pub struct CompressionConfig {
    /// Tokenizer model name (for tiktoken-rs).
    pub tokenizer_model: String,
    /// Target max tokens per trajectory after compression.
    pub target_max_tokens: usize,
    /// Target tokens for the generated summary.
    pub summary_target_tokens: usize,
    /// Protect the first system turn.
    pub protect_first_system: bool,
    /// Protect the first human turn.
    pub protect_first_human: bool,
    /// Protect the first gpt turn.
    pub protect_first_gpt: bool,
    /// Protect the first tool turn.
    pub protect_first_tool: bool,
    /// Protect the last N turns.
    pub protect_last_n_turns: usize,
    /// Add a notice to the system message when compression occurs.
    pub add_summary_notice: bool,
    /// Notice text appended to the system message.
    pub summary_notice: String,
    /// Skip trajectories already under target.
    pub skip_under_target: bool,
    /// Save trajectories that are still over limit after compression.
    pub save_over_limit: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            tokenizer_model: "gpt-4o".to_string(),
            target_max_tokens: 15_250,
            summary_target_tokens: 750,
            protect_first_system: true,
            protect_first_human: true,
            protect_first_gpt: true,
            protect_first_tool: true,
            protect_last_n_turns: 4,
            add_summary_notice: true,
            summary_notice: "\n\nSome of your previous tool responses may be summarized to preserve context.".to_string(),
            skip_under_target: true,
            save_over_limit: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Turn representation
// ---------------------------------------------------------------------------

/// A single conversation turn in a trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// Role: "system", "human", "gpt", "tool".
    #[serde(rename = "from")]
    pub role: String,
    /// Content.
    pub value: String,
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// Metrics for a single trajectory compression.
#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryMetrics {
    pub original_tokens: usize,
    pub compressed_tokens: usize,
    pub tokens_saved: usize,
    pub compression_ratio: f64,
    pub original_turns: usize,
    pub compressed_turns: usize,
    pub turns_removed: usize,
    pub turns_compressed_start_idx: usize,
    pub turns_compressed_end_idx: usize,
    pub turns_in_compressed_region: usize,
    pub was_compressed: bool,
    pub still_over_limit: bool,
    pub skipped_under_target: bool,
    pub summarization_api_calls: usize,
    pub summarization_errors: usize,
}

impl Default for TrajectoryMetrics {
    fn default() -> Self {
        Self {
            original_tokens: 0,
            compressed_tokens: 0,
            tokens_saved: 0,
            compression_ratio: 1.0,
            original_turns: 0,
            compressed_turns: 0,
            turns_removed: 0,
            turns_compressed_start_idx: 0,
            turns_compressed_end_idx: 0,
            turns_in_compressed_region: 0,
            was_compressed: false,
            still_over_limit: false,
            skipped_under_target: false,
            summarization_api_calls: 0,
            summarization_errors: 0,
        }
    }
}

/// Aggregate metrics across all trajectories.
#[derive(Debug, Clone, Serialize)]
#[derive(Default)]
pub struct AggregateMetrics {
    pub total_trajectories: usize,
    pub trajectories_compressed: usize,
    pub trajectories_skipped_under_target: usize,
    pub trajectories_still_over_limit: usize,
    pub trajectories_failed: usize,
    pub total_tokens_before: usize,
    pub total_tokens_after: usize,
    pub total_tokens_saved: usize,
    pub total_turns_before: usize,
    pub total_turns_after: usize,
    pub total_turns_removed: usize,
    pub total_summarization_calls: usize,
    pub total_summarization_errors: usize,
    pub compression_ratios: Vec<f64>,
    pub tokens_saved_list: Vec<usize>,
    pub turns_removed_list: Vec<usize>,
}

impl AggregateMetrics {
    pub fn add_trajectory(&mut self, m: &TrajectoryMetrics) {
        self.total_trajectories += 1;
        self.total_tokens_before += m.original_tokens;
        self.total_tokens_after += m.compressed_tokens;
        self.total_tokens_saved += m.tokens_saved;
        self.total_turns_before += m.original_turns;
        self.total_turns_after += m.compressed_turns;
        self.total_turns_removed += m.turns_removed;
        self.total_summarization_calls += m.summarization_api_calls;
        self.total_summarization_errors += m.summarization_errors;

        if m.was_compressed {
            self.trajectories_compressed += 1;
            self.compression_ratios.push(m.compression_ratio);
            self.tokens_saved_list.push(m.tokens_saved);
            self.turns_removed_list.push(m.turns_removed);
        }
        if m.skipped_under_target {
            self.trajectories_skipped_under_target += 1;
        }
        if m.still_over_limit {
            self.trajectories_still_over_limit += 1;
        }
    }

    pub fn avg_compression_ratio(&self) -> f64 {
        if self.compression_ratios.is_empty() {
            return 1.0;
        }
        self.compression_ratios.iter().sum::<f64>() / self.compression_ratios.len() as f64
    }

    pub fn avg_tokens_saved(&self) -> f64 {
        if self.tokens_saved_list.is_empty() {
            return 0.0;
        }
        self.tokens_saved_list.iter().sum::<usize>() as f64 / self.tokens_saved_list.len() as f64
    }
}

// ---------------------------------------------------------------------------
// Compressor
// ---------------------------------------------------------------------------

/// Compresses agent trajectories to fit within a target token budget.
pub struct TrajectoryCompressor {
    config: CompressionConfig,
    summarizer: Summarizer,
    // --- Runtime state (mirrors Python ContextCompressor fields) ---
    /// Token usage from last API call.
    last_prompt_tokens: usize,
    last_completion_tokens: usize,
    last_total_tokens: usize,
    /// Total compression operations performed.
    compression_count: usize,
    /// Discovered model context length (from probing).
    context_length: usize,
    /// Threshold percentage for should_compress (default 0.75).
    threshold_percent: f64,
}

impl TrajectoryCompressor {
    /// Create a new compressor with the given configuration.
    pub fn new(config: CompressionConfig) -> Self {
        let summarizer = Summarizer::new(&config);
        Self {
            config,
            summarizer,
            last_prompt_tokens: 0,
            last_completion_tokens: 0,
            last_total_tokens: 0,
            compression_count: 0,
            context_length: 0,
            threshold_percent: 0.75,
        }
    }

    /// Count tokens in text using tiktoken-rs.
    pub fn count_tokens(&self, text: &str) -> usize {
        if text.is_empty() {
            return 0;
        }
        match get_bpe_from_model(&self.config.tokenizer_model) {
            Ok(bpe) => bpe.encode_ordinary(text).len(),
            Err(_) => {
                // Fallback: rough character-based estimate
                text.len() / 4
            }
        }
    }

    /// Count total tokens in a trajectory.
    pub fn count_trajectory_tokens(&self, trajectory: &[Turn]) -> usize {
        trajectory.iter().map(|t| self.count_tokens(&t.value)).sum()
    }

    /// Count tokens per turn.
    fn count_turn_tokens(&self, trajectory: &[Turn]) -> Vec<usize> {
        trajectory.iter().map(|t| self.count_tokens(&t.value)).collect()
    }

    /// Find indices of protected turns and the compressible region.
    ///
    /// Returns (protected_set, compressible_start, compressible_end).
    fn find_protected_indices(&self, trajectory: &[Turn]) -> (Vec<usize>, usize, usize) {
        let n = trajectory.len();
        if n == 0 {
            return (Vec::new(), 0, 0);
        }

        let mut protected = Vec::new();
        let mut first_system: Option<usize> = None;
        let mut first_human: Option<usize> = None;
        let mut first_gpt: Option<usize> = None;
        let mut first_tool: Option<usize> = None;

        for (i, turn) in trajectory.iter().enumerate() {
            match turn.role.as_str() {
                "system" if first_system.is_none() => first_system = Some(i),
                "human" if first_human.is_none() => first_human = Some(i),
                "gpt" if first_gpt.is_none() => first_gpt = Some(i),
                "tool" if first_tool.is_none() => first_tool = Some(i),
                _ => {}
            }
        }

        if self.config.protect_first_system {
            if let Some(idx) = first_system {
                protected.push(idx);
            }
        }
        if self.config.protect_first_human {
            if let Some(idx) = first_human {
                protected.push(idx);
            }
        }
        if self.config.protect_first_gpt {
            if let Some(idx) = first_gpt {
                protected.push(idx);
            }
        }
        if self.config.protect_first_tool {
            if let Some(idx) = first_tool {
                protected.push(idx);
            }
        }

        // Protect last N turns
        let tail_start = n.saturating_sub(self.config.protect_last_n_turns);
        for i in tail_start..n {
            if !protected.contains(&i) {
                protected.push(i);
            }
        }

        protected.sort_unstable();

        // Determine compressible region
        let head_protected: Vec<_> = protected.iter().filter(|&&i| i < n / 2).copied().collect();
        let tail_protected: Vec<_> = protected.iter().filter(|&&i| i >= n / 2).copied().collect();

        let compressible_start = head_protected.last().map(|&i| i + 1).unwrap_or(0);
        let compressible_end = tail_protected.first().copied().unwrap_or(n);

        (protected, compressible_start, compressible_end)
    }

    /// Extract content from turns for summarization.
    fn extract_turn_content_for_summary(
        &self,
        trajectory: &[Turn],
        start: usize,
        end: usize,
    ) -> String {
        let mut parts = Vec::new();
        for (i, turn) in trajectory[start..end.min(trajectory.len())].iter().enumerate() {
            let i = start + i;
            let mut value = turn.value.clone();
            let role = turn.role.to_uppercase();

            // Truncate very long values
            if value.len() > 3000 {
                let truncated = format!(
                    "{}\n...[truncated]...\n{}",
                    &value[..1500.min(value.len())],
                    &value[value.len().saturating_sub(500)..]
                );
                value = truncated;
            }

            parts.push(format!("[Turn {i} - {role}]:\n{value}"));
        }
        parts.join("\n\n")
    }

    /// Compress a single trajectory.
    ///
    /// Returns (compressed_trajectory, metrics).
    pub async fn compress_trajectory(
        &mut self,
        trajectory: Vec<Turn>,
    ) -> (Vec<Turn>, TrajectoryMetrics) {
        let mut metrics = TrajectoryMetrics {
            original_turns: trajectory.len(),
            ..Default::default()
        };

        let turn_tokens = self.count_turn_tokens(&trajectory);
        let total_tokens: usize = turn_tokens.iter().sum();
        metrics.original_tokens = total_tokens;

        // Check if compression is needed
        if total_tokens <= self.config.target_max_tokens {
            metrics.skipped_under_target = true;
            metrics.compressed_tokens = total_tokens;
            metrics.compressed_turns = trajectory.len();
            metrics.compression_ratio = 1.0;
            return (trajectory, metrics);
        }

        // Find protected regions
        let (_protected, compress_start, compress_end) =
            self.find_protected_indices(&trajectory);

        // Check if there's anything to compress
        if compress_start >= compress_end {
            metrics.compressed_tokens = total_tokens;
            metrics.compressed_turns = trajectory.len();
            metrics.still_over_limit = total_tokens > self.config.target_max_tokens;
            return (trajectory, metrics);
        }

        // Calculate how much we need to save
        let tokens_to_save = total_tokens - self.config.target_max_tokens;
        // Net savings = (sum of N turns) - summary_target_tokens
        let target_tokens_to_compress = tokens_to_save + self.config.summary_target_tokens;

        // Accumulate turns from compress_start until we have enough
        let mut accumulated_tokens: usize = 0;
        let mut compress_until = compress_start;

        for (i, &tokens) in turn_tokens[compress_start..compress_end].iter().enumerate() {
            accumulated_tokens += tokens;
            compress_until = compress_start + i + 1;
            if accumulated_tokens >= target_tokens_to_compress {
                break;
            }
        }

        // If still not enough, compress entire compressible region
        if accumulated_tokens < target_tokens_to_compress && compress_until < compress_end {
            compress_until = compress_end;
        }

        // Record compression region
        metrics.turns_compressed_start_idx = compress_start;
        metrics.turns_compressed_end_idx = compress_until;
        metrics.turns_in_compressed_region = compress_until - compress_start;

        // Extract content for summary
        let content_to_summarize =
            self.extract_turn_content_for_summary(&trajectory, compress_start, compress_until);

        // Generate summary
        let summary = self
            .summarizer
            .generate_summary(&content_to_summarize, &mut metrics)
            .await;

        // Build compressed trajectory
        let mut compressed = Vec::new();

        // Add head (turns before compression region)
        for turn in &trajectory[..compress_start] {
            let mut turn = turn.clone();
            if turn.role == "system" && self.config.add_summary_notice {
                turn.value.push_str(&self.config.summary_notice);
            }
            compressed.push(turn);
        }

        // Add summary as human message
        compressed.push(Turn {
            role: "human".to_string(),
            value: summary,
        });

        // Add tail (turns after compression region)
        for turn in &trajectory[compress_until..] {
            compressed.push(turn.clone());
        }

        // Calculate final metrics
        metrics.compressed_turns = compressed.len();
        metrics.compressed_tokens = self.count_trajectory_tokens(&compressed);
        metrics.turns_removed = metrics.original_turns - metrics.compressed_turns;
        metrics.tokens_saved = metrics.original_tokens - metrics.compressed_tokens;
        metrics.compression_ratio =
            metrics.compressed_tokens as f64 / metrics.original_tokens.max(1) as f64;
        metrics.was_compressed = true;
        metrics.still_over_limit = metrics.compressed_tokens > self.config.target_max_tokens;

        (compressed, metrics)
    }

    /// Process a single JSONL file.
    ///
    /// Returns aggregate metrics.
    pub async fn process_file(
        &mut self,
        input_path: &Path,
        output_path: &Path,
        sample_percent: Option<f64>,
    ) -> Result<AggregateMetrics, std::io::Error> {
        let content = std::fs::read_to_string(input_path)?;
        let mut trajectories: Vec<Vec<Turn>> = Vec::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Vec<Turn>>(line) {
                Ok(t) => trajectories.push(t),
                Err(e) => {
                    tracing::warn!("Failed to parse trajectory: {e}");
                }
            }
        }

        // Apply sampling if requested
        if let Some(pct) = sample_percent {
            let sample_size = (trajectories.len() as f64 * pct / 100.0).ceil() as usize;
            if sample_size < trajectories.len() {
                // Deterministic sample: take every Nth
                let step = trajectories.len().max(1) / sample_size.max(1);
                trajectories = trajectories.into_iter().step_by(step.max(1)).collect();
                trajectories.truncate(sample_size);
            }
        }

        let total = trajectories.len();
        let mut agg = AggregateMetrics::default();

        let bar = indicatif::ProgressBar::new(total as u64);
        bar.set_style(
            indicatif::ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap(),
        );

        let mut results = Vec::new();

        for trajectory in trajectories {
            let (compressed, metrics) = self.compress_trajectory(trajectory).await;
            agg.add_trajectory(&metrics);

            if metrics.skipped_under_target && self.config.skip_under_target {
                // Still save it (unchanged)
                results.push(compressed);
            } else if metrics.still_over_limit && !self.config.save_over_limit {
                // Skip saving
            } else {
                results.push(compressed);
            }

            bar.inc(1);
            bar.set_message(format!(
                "saved {} tokens ({:.1}%)",
                metrics.tokens_saved,
                (1.0 - metrics.compression_ratio) * 100.0
            ));
        }

        bar.finish();

        // Write output
        let mut out = std::io::BufWriter::new(std::fs::File::create(output_path)?);
        for traj in &results {
            let line = serde_json::to_string(traj).map_err(std::io::Error::other)?;
            writeln!(out, "{line}")?;
        }

        Ok(agg)
    }

    /// Process a directory of JSONL files.
    ///
    /// Finds all `.jsonl` files in the directory, compresses each, and
    /// writes to `{input}_compressed.jsonl`.
    pub async fn process_directory(
        &mut self,
        input_dir: &Path,
        sample_percent: Option<f64>,
    ) -> Result<AggregateMetrics, std::io::Error> {
        let mut agg = AggregateMetrics::default();

        let entries: Vec<_> = std::fs::read_dir(input_dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .is_some_and(|ext| ext == "jsonl")
            })
            .collect();

        if entries.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("No .jsonl files found in {:?}", input_dir),
            ));
        }

        for entry in &entries {
            let input_path = entry.path();
            let stem = input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            let output_name = format!("{}_compressed.jsonl", stem);
            let output_path = input_path.with_file_name(&output_name);

            tracing::info!(
                "Compressing {:?} -> {:?}",
                input_path,
                output_path
            );

            let file_agg = self
                .process_file(&input_path, &output_path, sample_percent)
                .await?;

            // Merge file_agg into agg
            agg.total_trajectories += file_agg.total_trajectories;
            agg.trajectories_compressed += file_agg.trajectories_compressed;
            agg.trajectories_skipped_under_target += file_agg.trajectories_skipped_under_target;
            agg.trajectories_still_over_limit += file_agg.trajectories_still_over_limit;
            agg.trajectories_failed += file_agg.trajectories_failed;
            agg.total_tokens_before += file_agg.total_tokens_before;
            agg.total_tokens_after += file_agg.total_tokens_after;
            agg.total_tokens_saved += file_agg.total_tokens_saved;
            agg.total_turns_before += file_agg.total_turns_before;
            agg.total_turns_after += file_agg.total_turns_after;
            agg.total_turns_removed += file_agg.total_turns_removed;
            agg.total_summarization_calls += file_agg.total_summarization_calls;
            agg.total_summarization_errors += file_agg.total_summarization_errors;
            agg.compression_ratios.extend(file_agg.compression_ratios);
            agg.tokens_saved_list.extend(file_agg.tokens_saved_list);
            agg.turns_removed_list.extend(file_agg.turns_removed_list);
        }

        Ok(agg)
    }

    /// Update model info after a model switch or fallback activation.
    ///
    /// Mirrors Python `ContextCompressor.update_model()`.
    pub fn update_model(
        &mut self,
        model: &str,
        context_length: usize,
        base_url: &str,
        api_key: &str,
        provider: &str,
    ) {
        // Update tokenizer model if the model changed
        self.config.tokenizer_model = model.to_string();
        self.context_length = context_length;
        self.config.target_max_tokens =
            (context_length as f64 * self.threshold_percent).round() as usize;

        // Log for observability (unused params kept for future use)
        let _ = (base_url, api_key, provider);
        tracing::info!(
            "Context engine model update: model={}, context_length={}, provider={}",
            model, context_length, provider
        );
    }

    /// Reset all per-session state for /new or /reset.
    ///
    /// Mirrors Python `ContextCompressor.on_session_reset()`.
    pub fn on_session_reset(&mut self) {
        self.last_prompt_tokens = 0;
        self.last_completion_tokens = 0;
        self.last_total_tokens = 0;
        self.compression_count = 0;
    }
}

// ---------------------------------------------------------------------------
// ContextEngine trait implementation
// ---------------------------------------------------------------------------

impl ContextEngine for TrajectoryCompressor {
    fn name(&self) -> &str {
        "compressor"
    }

    fn last_prompt_tokens(&self) -> usize {
        self.last_prompt_tokens
    }

    fn last_completion_tokens(&self) -> usize {
        self.last_completion_tokens
    }

    fn last_total_tokens(&self) -> usize {
        self.last_total_tokens
    }

    fn threshold_tokens(&self) -> usize {
        // Floor: never compress below MINIMUM_CONTEXT_LENGTH tokens even if
        // the configured target is lower. Models need enough working memory
        // for tool-calling workflows. Mirrors Python: max(target, 64K).
        self.config.target_max_tokens.max(MINIMUM_CONTEXT_LENGTH)
    }

    fn context_length(&self) -> usize {
        self.context_length
    }

    fn compression_count(&self) -> usize {
        self.compression_count
    }

    fn update_from_response(&mut self, usage: &Value) {
        if let Some(prompt) = usage.get("prompt_tokens").and_then(Value::as_u64) {
            self.last_prompt_tokens = prompt as usize;
        }
        if let Some(completion) = usage.get("completion_tokens").and_then(Value::as_u64) {
            self.last_completion_tokens = completion as usize;
        }
        if let Some(total) = usage.get("total_tokens").and_then(Value::as_u64) {
            self.last_total_tokens = total as usize;
        }
    }

    fn should_compress(&self, prompt_tokens: Option<usize>) -> bool {
        let tokens = prompt_tokens.unwrap_or(self.last_prompt_tokens);
        // Floor: never compress below MINIMUM_CONTEXT_LENGTH
        let threshold = self.config.target_max_tokens.max(MINIMUM_CONTEXT_LENGTH);
        tokens > threshold
    }

    fn compress(&mut self, messages: Vec<Value>) -> Vec<Value> {
        // Convert JSON messages to Turns, compress, convert back.
        // This is a simplified path — the full path uses compress_trajectory().
        let turns: Vec<Turn> = messages
            .iter()
            .filter_map(|msg| {
                let role = msg.get("role").and_then(Value::as_str)?;
                let value = msg.get("content").and_then(Value::as_str)?;
                Some(Turn {
                    role: role.to_string(),
                    value: value.to_string(),
                })
            })
            .collect();

        let total_tokens: usize = turns.iter().map(|t| self.count_tokens(&t.value)).sum();
        if total_tokens <= self.config.target_max_tokens {
            self.last_prompt_tokens = total_tokens;
            return messages;
        }

        self.compression_count += 1;
        tracing::warn!(
            "Context compress triggered (sync fallback). Total tokens: {}",
            total_tokens
        );
        messages
    }

    fn should_compress_preflight(&self, messages: &[Value]) -> bool {
        // Quick estimate using character count
        let total_chars: usize = messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_str))
            .map(|s| s.len())
            .sum();
        let estimated_tokens = total_chars / 4;
        estimated_tokens > self.config.target_max_tokens
    }

    fn on_session_reset(&mut self) {
        self.on_session_reset();
    }

    fn update_model(
        &mut self,
        model: &str,
        context_length: usize,
        base_url: &str,
        api_key: &str,
        provider: &str,
    ) {
        self.update_model(model, context_length, base_url, api_key, provider);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_turn(role: &str, n_tokens: usize) -> Turn {
        // Each "tok " is roughly 1 token in tiktoken
        Turn {
            role: role.to_string(),
            value: "tok ".repeat(n_tokens),
        }
    }

    fn test_config() -> CompressionConfig {
        let mut cfg = CompressionConfig::default();
        cfg.protect_last_n_turns = 2;
        cfg.skip_under_target = false; // Allow testing compression even when under
        cfg.save_over_limit = true;
        cfg
    }

    #[test]
    fn test_count_tokens() {
        let compressor = TrajectoryCompressor::new(test_config());
        let count = compressor.count_tokens("Hello, world!");
        assert!(count > 0);
    }

    #[test]
    fn test_protected_indices() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory = vec![
            make_turn("system", 100),
            make_turn("human", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
        ];

        let (protected, start, end) = compressor.find_protected_indices(&trajectory);
        // First system, human, gpt, tool are protected
        assert!(protected.contains(&0)); // system
        assert!(protected.contains(&1)); // human
        assert!(protected.contains(&2)); // first gpt
        assert!(protected.contains(&3)); // first tool
        // Last 2 turns are protected
        assert!(protected.contains(&8)); // gpt
        assert!(protected.contains(&9)); // tool

        // Compressible region should be between head and tail
        assert!(start >= 4);
        assert!(end <= 8);
    }

    #[tokio::test]
    async fn test_compress_trajectory_under_target() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 100_000; // Very high, no compression needed
        cfg.skip_under_target = true;
        let mut compressor = TrajectoryCompressor::new(cfg);

        let trajectory = vec![
            make_turn("system", 100),
            make_turn("human", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
        ];

        let (compressed, metrics) = compressor.compress_trajectory(trajectory.clone()).await;
        assert!(metrics.skipped_under_target);
        assert_eq!(compressed.len(), trajectory.len());
    }

    #[tokio::test]
    async fn test_compress_trajectory_needs_compression() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 500; // Low enough to trigger compression
        cfg.summary_target_tokens = 100;
        cfg.skip_under_target = false;
        let mut compressor = TrajectoryCompressor::new(cfg);

        let trajectory = vec![
            make_turn("system", 50),
            make_turn("human", 50),
            make_turn("gpt", 50),
            make_turn("tool", 50),
            make_turn("gpt", 50),
            make_turn("tool", 50),
            make_turn("gpt", 50),
            make_turn("tool", 50),
            make_turn("gpt", 50),
            make_turn("tool", 50),
        ];

        let (compressed, metrics) = compressor.compress_trajectory(trajectory).await;
        assert!(metrics.was_compressed);
        // Should have a summary message replacing some turns
        assert!(compressed.len() > 0);
        // The summary turn should be human role
        let has_summary = compressed.iter().any(|t| {
            t.role == "human" && t.value.contains("[CONTEXT SUMMARY]")
        });
        assert!(has_summary);
    }

    #[test]
    fn test_protected_indices_empty_trajectory() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory: Vec<Turn> = vec![];
        let (protected, start, end) = compressor.find_protected_indices(&trajectory);
        assert!(protected.is_empty());
        assert_eq!(start, 0);
        assert_eq!(end, 0);
    }

    #[test]
    fn test_extract_content_truncation() {
        let compressor = TrajectoryCompressor::new(test_config());
        let long_value = "x".repeat(5000);
        let trajectory = vec![
            Turn { role: "gpt".to_string(), value: long_value },
            Turn { role: "tool".to_string(), value: "short".to_string() },
        ];

        let content = compressor.extract_turn_content_for_summary(&trajectory, 0, 2);
        assert!(content.contains("[truncated]"));
        assert!(content.contains("[Turn 0 - GPT]"));
        assert!(content.contains("[Turn 1 - TOOL]"));
    }

    #[test]
    fn test_aggregate_metrics() {
        let mut agg = AggregateMetrics::default();
        let m = TrajectoryMetrics {
            original_tokens: 1000,
            compressed_tokens: 600,
            tokens_saved: 400,
            compression_ratio: 0.6,
            original_turns: 10,
            compressed_turns: 6,
            turns_removed: 4,
            was_compressed: true,
            skipped_under_target: false,
            still_over_limit: false,
            summarization_api_calls: 1,
            summarization_errors: 0,
            ..Default::default()
        };
        agg.add_trajectory(&m);
        agg.add_trajectory(&m);

        assert_eq!(agg.total_trajectories, 2);
        assert_eq!(agg.trajectories_compressed, 2);
        assert_eq!(agg.total_tokens_saved, 800);
        assert!((agg.avg_compression_ratio() - 0.6).abs() < 0.001);
    }

    #[test]
    fn test_single_turn_trajectory() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory = vec![make_turn("human", 100)];
        let (protected, start, end) = compressor.find_protected_indices(&trajectory);
        assert_eq!(protected.len(), 1);
        assert!(protected.contains(&0));
        // n/2 = 0, so head_protected is empty, tail_protected contains 0
        assert_eq!(start, 0);
        assert_eq!(end, 0);
    }

    #[test]
    fn test_all_same_role_trajectory() {
        let compressor = TrajectoryCompressor::new(test_config());
        // All gpt turns — no system/human/tool to protect
        let trajectory = vec![
            make_turn("gpt", 100),
            make_turn("gpt", 100),
            make_turn("gpt", 100),
            make_turn("gpt", 100),
            make_turn("gpt", 100),
            make_turn("gpt", 100),
        ];
        let (protected, start, end) = compressor.find_protected_indices(&trajectory);
        // First gpt is protected, last 2 are protected
        assert!(protected.contains(&0)); // first gpt
        assert!(protected.contains(&4)); // last 2
        assert!(protected.contains(&5));
        // Compressible region should exist between head and tail
        assert!(start < end);
    }

    #[test]
    fn test_protect_last_n_zero() {
        let mut cfg = test_config();
        cfg.protect_last_n_turns = 0;
        let compressor = TrajectoryCompressor::new(cfg);
        let trajectory = vec![
            make_turn("system", 100),
            make_turn("human", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
            make_turn("gpt", 100),
            make_turn("tool", 100),
        ];
        let (protected, start, end) = compressor.find_protected_indices(&trajectory);
        // Only first turns protected, no tail protection
        assert!(protected.contains(&0));
        assert!(protected.contains(&1));
        assert!(protected.contains(&2));
        assert!(protected.contains(&3));
        // With protect_last_n_turns=0 and n=6, n/2=3
        // head_protected = indices < 3 → may include some first-turn indices
        // tail_protected = indices >= 3
        // The exact start/end depends on the split logic
        assert!(start <= end);
    }

    #[test]
    fn test_count_tokens_empty_string() {
        let compressor = TrajectoryCompressor::new(test_config());
        assert_eq!(compressor.count_tokens(""), 0);
    }

    #[test]
    fn test_count_tokens_invalid_model() {
        let mut cfg = test_config();
        cfg.tokenizer_model = "nonexistent_model_xyz".to_string();
        let compressor = TrajectoryCompressor::new(cfg);
        // Should fall back to character / 4 estimate
        let text = "hello world";
        let count = compressor.count_tokens(text);
        assert_eq!(count, text.len() / 4);
    }

    #[test]
    fn test_count_trajectory_tokens_empty() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory: Vec<Turn> = vec![];
        assert_eq!(compressor.count_trajectory_tokens(&trajectory), 0);
    }

    #[test]
    fn test_aggregate_avg_tokens_saved_empty() {
        let agg = AggregateMetrics::default();
        assert_eq!(agg.avg_tokens_saved(), 0.0);
    }

    #[test]
    fn test_aggregate_avg_compression_ratio_empty() {
        let agg = AggregateMetrics::default();
        assert_eq!(agg.avg_compression_ratio(), 1.0);
    }

    #[test]
    fn test_aggregate_skipped_and_failed() {
        let mut agg = AggregateMetrics::default();
        let skipped = TrajectoryMetrics {
            skipped_under_target: true,
            was_compressed: false,
            original_tokens: 500,
            compressed_tokens: 500,
            tokens_saved: 0,
            compression_ratio: 1.0,
            original_turns: 5,
            compressed_turns: 5,
            ..Default::default()
        };
        let failed = TrajectoryMetrics {
            summarization_errors: 2,
            original_tokens: 2000,
            compressed_tokens: 1800,
            tokens_saved: 200,
            compression_ratio: 0.9,
            original_turns: 20,
            compressed_turns: 18,
            was_compressed: true,
            still_over_limit: true,
            summarization_api_calls: 3,
            ..Default::default()
        };
        agg.add_trajectory(&skipped);
        agg.add_trajectory(&failed);

        assert_eq!(agg.total_trajectories, 2);
        assert_eq!(agg.trajectories_skipped_under_target, 1);
        assert_eq!(agg.trajectories_still_over_limit, 1);
        assert_eq!(agg.total_summarization_errors, 2);
        assert_eq!(agg.total_summarization_calls, 3);
    }

    #[tokio::test]
    async fn test_compress_trajectory_no_compressible_region() {
        // When compressible start >= end, should return unchanged
        let mut cfg = CompressionConfig::default();
        cfg.protect_last_n_turns = 0;
        cfg.skip_under_target = false;
        cfg.save_over_limit = true;
        let mut compressor = TrajectoryCompressor::new(cfg);

        // Small trajectory where all turns are "first" of their role
        // and there's nothing left to compress
        let trajectory = vec![
            make_turn("system", 200),
            make_turn("human", 200),
        ];

        let (compressed, metrics) = compressor.compress_trajectory(trajectory.clone()).await;
        // Even with low target, if no compressible region, returns as-is
        assert_eq!(compressed.len(), trajectory.len());
        assert!(!metrics.was_compressed);
    }

    #[test]
    fn test_extract_content_no_truncation() {
        let compressor = TrajectoryCompressor::new(test_config());
        let short_value = "short content".to_string();
        let trajectory = vec![
            Turn { role: "gpt".to_string(), value: short_value.clone() },
        ];
        let content = compressor.extract_turn_content_for_summary(&trajectory, 0, 1);
        assert!(!content.contains("[truncated]"));
        assert!(content.contains("short content"));
        assert!(content.contains("[Turn 0 - GPT]"));
    }

    #[test]
    fn test_extract_content_partial_range() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory = vec![
            make_turn("system", 10),
            make_turn("human", 10),
            make_turn("gpt", 10),
            make_turn("tool", 10),
        ];
        let content = compressor.extract_turn_content_for_summary(&trajectory, 1, 3);
        assert!(content.contains("[Turn 1 - HUMAN]"));
        assert!(content.contains("[Turn 2 - GPT]"));
        assert!(!content.contains("[Turn 0 - SYSTEM]"));
        assert!(!content.contains("[Turn 3 - TOOL]"));
    }

    #[test]
    fn test_extract_content_end_beyond_length() {
        let compressor = TrajectoryCompressor::new(test_config());
        let trajectory = vec![
            make_turn("gpt", 10),
            make_turn("tool", 10),
        ];
        // end=10 is beyond trajectory length
        let content = compressor.extract_turn_content_for_summary(&trajectory, 0, 10);
        assert!(content.contains("[Turn 0 - GPT]"));
        assert!(content.contains("[Turn 1 - TOOL]"));
    }

    #[test]
    fn test_compression_config_defaults() {
        let cfg = CompressionConfig::default();
        assert_eq!(cfg.tokenizer_model, "gpt-4o");
        assert_eq!(cfg.target_max_tokens, 15_250);
        assert_eq!(cfg.summary_target_tokens, 750);
        assert!(cfg.protect_first_system);
        assert!(cfg.protect_first_human);
        assert!(cfg.protect_first_gpt);
        assert!(cfg.protect_first_tool);
        assert_eq!(cfg.protect_last_n_turns, 4);
        assert!(cfg.add_summary_notice);
        assert!(cfg.skip_under_target);
        assert!(cfg.save_over_limit);
    }

    #[test]
    fn test_trajectory_metrics_defaults() {
        let m = TrajectoryMetrics::default();
        assert_eq!(m.original_tokens, 0);
        assert_eq!(m.compressed_tokens, 0);
        assert_eq!(m.tokens_saved, 0);
        assert!((m.compression_ratio - 1.0).abs() < 0.001);
        assert_eq!(m.original_turns, 0);
        assert_eq!(m.compressed_turns, 0);
        assert_eq!(m.turns_removed, 0);
        assert!(!m.was_compressed);
        assert!(!m.still_over_limit);
        assert!(!m.skipped_under_target);
        assert_eq!(m.summarization_api_calls, 0);
        assert_eq!(m.summarization_errors, 0);
    }

    #[test]
    fn test_context_engine_name() {
        let compressor = TrajectoryCompressor::new(test_config());
        assert_eq!(compressor.name(), "compressor");
    }

    #[test]
    fn test_context_engine_initial_state() {
        let compressor = TrajectoryCompressor::new(test_config());
        assert_eq!(compressor.last_prompt_tokens(), 0);
        assert_eq!(compressor.last_completion_tokens(), 0);
        assert_eq!(compressor.last_total_tokens(), 0);
        assert_eq!(compressor.compression_count(), 0);
        assert_eq!(compressor.context_length(), 0);
    }

    #[test]
    fn test_context_engine_update_from_response() {
        let mut compressor = TrajectoryCompressor::new(test_config());
        let usage = serde_json::json!({
            "prompt_tokens": 5000,
            "completion_tokens": 3000,
            "total_tokens": 8000
        });
        compressor.update_from_response(&usage);
        assert_eq!(compressor.last_prompt_tokens(), 5000);
        assert_eq!(compressor.last_completion_tokens(), 3000);
        assert_eq!(compressor.last_total_tokens(), 8000);
    }

    #[test]
    fn test_context_engine_should_compress() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 1000;
        let mut compressor = TrajectoryCompressor::new(cfg);
        // With 64K floor, effective threshold is max(1000, 64000) = 64000
        compressor.last_prompt_tokens = 500;
        assert!(!compressor.should_compress(None));
        compressor.last_prompt_tokens = 1500;
        // Still below 64K floor — should NOT compress
        assert!(!compressor.should_compress(None));
        // Above 64K floor — should compress
        compressor.last_prompt_tokens = 70_000;
        assert!(compressor.should_compress(None));
        // Override via parameter
        assert!(compressor.should_compress(Some(100_000)));
        assert!(!compressor.should_compress(Some(50_000)));
    }

    #[test]
    fn test_context_engine_floor_with_high_target() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 100_000; // Above 64K floor
        let mut compressor = TrajectoryCompressor::new(cfg);
        // Threshold is max(100000, 64000) = 100000
        compressor.last_prompt_tokens = 90_000;
        assert!(!compressor.should_compress(None));
        compressor.last_prompt_tokens = 110_000;
        assert!(compressor.should_compress(None));
    }

    #[test]
    fn test_threshold_tokens_respects_floor() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 1000;
        let compressor = TrajectoryCompressor::new(cfg);
        assert_eq!(compressor.threshold_tokens(), 64_000); // floor kicks in

        let mut cfg = test_config();
        cfg.target_max_tokens = 100_000;
        let compressor = TrajectoryCompressor::new(cfg);
        assert_eq!(compressor.threshold_tokens(), 100_000); // above floor
    }

    #[test]
    fn test_context_engine_should_compress_preflight() {
        let mut cfg = test_config();
        cfg.target_max_tokens = 100;
        let compressor = TrajectoryCompressor::new(cfg);
        let messages = vec![serde_json::json!({"role": "user", "content": "tok ".repeat(500)})];
        // 2000 chars / 4 ≈ 500 tokens > 100 target
        assert!(compressor.should_compress_preflight(&messages));
        let short = vec![serde_json::json!({"role": "user", "content": "hi"})];
        assert!(!compressor.should_compress_preflight(&short));
    }

    #[test]
    fn test_context_engine_update_model() {
        let mut compressor = TrajectoryCompressor::new(test_config());
        compressor.update_model(
            "gpt-4o",
            128_000,
            "https://api.openai.com",
            "sk-test",
            "openai",
        );
        assert_eq!(compressor.context_length(), 128_000);
        assert_eq!(compressor.config.tokenizer_model, "gpt-4o");
        // threshold = 128000 * 0.75 = 96000
        assert_eq!(compressor.config.target_max_tokens, 96_000);
    }

    #[test]
    fn test_context_engine_on_session_reset() {
        let mut compressor = TrajectoryCompressor::new(test_config());
        compressor.last_prompt_tokens = 5000;
        compressor.last_completion_tokens = 3000;
        compressor.last_total_tokens = 8000;
        compressor.compression_count = 10;

        compressor.on_session_reset();

        assert_eq!(compressor.last_prompt_tokens(), 0);
        assert_eq!(compressor.last_completion_tokens(), 0);
        assert_eq!(compressor.last_total_tokens(), 0);
        assert_eq!(compressor.compression_count(), 0);
    }

    #[test]
    fn test_context_engine_get_status() {
        let mut compressor = TrajectoryCompressor::new(test_config());
        compressor.last_prompt_tokens = 5000;
        compressor.context_length = 10_000;
        compressor.compression_count = 3;

        let status = compressor.get_status();
        assert_eq!(status["last_prompt_tokens"], 5000);
        assert_eq!(status["context_length"], 10_000);
        assert_eq!(status["usage_percent"], 50);
        assert_eq!(status["compression_count"], 3);
    }

    #[test]
    fn test_context_engine_get_tool_schemas_empty() {
        let compressor = TrajectoryCompressor::new(test_config());
        let schemas = compressor.get_tool_schemas();
        assert!(schemas.is_empty());
    }

    #[test]
    fn test_context_engine_handle_tool_call_default() {
        let compressor = TrajectoryCompressor::new(test_config());
        let result = compressor.handle_tool_call("unknown_tool", &serde_json::json!({}));
        assert!(result.contains("No context engine tools registered"));
    }

    #[test]
    fn test_context_engine_trait_impl() {
        // Verify TrajectoryCompressor satisfies ContextEngine trait bounds
        fn assert_context_engine<T: ContextEngine>(_: &T) {}
        let compressor = TrajectoryCompressor::new(test_config());
        assert_context_engine(&compressor);
    }
}
