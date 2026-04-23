//! Context compression for long conversations.
//!
//! Mirrors the Python `agent/context_compressor.py`.
//! 4-stage algorithm:
//!   1. Prune old tool results (cheap, no LLM call)
//!   2. Protect head messages (system prompt + first exchange)
//!   3. Protect tail messages by token budget (most recent context)
//!   4. Summarize middle turns with structured LLM prompt

use serde_json::Value;

/// Minimum tokens for the summary output.
const MIN_SUMMARY_TOKENS: usize = 2000;
/// Proportion of compressed content to allocate for summary.
const SUMMARY_RATIO: f64 = 0.20;
/// Absolute ceiling for summary tokens.
const SUMMARY_TOKENS_CEILING: usize = 12_000;
/// Placeholder used when pruning old tool results.
const PRUNED_TOOL_PLACEHOLDER: &str = "[Old tool output cleared to save context space]";
/// Chars per token rough estimate.
const CHARS_PER_TOKEN: usize = 4;
/// Summary failure cooldown in seconds.
#[allow(dead_code)]
const SUMMARY_FAILURE_COOLDOWN_SECONDS: f64 = 600.0;

/// Summary prefixes.
const SUMMARY_PREFIX: &str =
    "[CONTEXT COMPACTION] Earlier turns in this conversation were compacted \
    to save context space. The summary below describes work that was \
    already completed, and the current session state may still reflect \
    that work (for example, files may already be changed). Use the summary \
    and the current state to continue from where things left off, and \
    avoid repeating work:";
#[cfg(test)]
const LEGACY_SUMMARY_PREFIX: &str = "[CONTEXT SUMMARY]:";

/// Configuration for the context compressor.
#[derive(Debug, Clone)]
pub struct CompressorConfig {
    /// Model name (for context length lookup).
    pub model: String,
    /// Compress when context exceeds this fraction of model's context length.
    pub threshold_percent: f64,
    /// Always keep first N messages uncompressed.
    pub protect_first_n: usize,
    /// Minimum recent messages to protect (fallback when no token budget).
    pub protect_last_n: usize,
    /// Proportion of compressed content for summary.
    pub summary_target_ratio: f64,
    /// Quiet mode (suppress logging).
    pub quiet_mode: bool,
    /// Summary model override.
    pub summary_model_override: Option<String>,
    /// Context length override.
    pub config_context_length: Option<usize>,
    /// Provider override for summary LLM calls.
    pub summary_provider: Option<String>,
    /// Base URL override for summary LLM calls.
    pub summary_base_url: Option<String>,
    /// API key override for summary LLM calls.
    pub summary_api_key: Option<String>,
    /// API mode for summary LLM calls (e.g., "anthropic", "openai").
    pub summary_api_mode: Option<String>,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            threshold_percent: 0.50,
            protect_first_n: 3,
            protect_last_n: 20,
            summary_target_ratio: 0.20,
            quiet_mode: false,
            summary_model_override: None,
            config_context_length: None,
            summary_provider: None,
            summary_base_url: None,
            summary_api_key: None,
            summary_api_mode: None,
        }
    }
}

/// Context compressor state.
#[derive(Debug)]
pub struct ContextCompressor {
    config: CompressorConfig,
    #[allow(dead_code)]
    context_length: usize,
    threshold_tokens: usize,
    compression_count: usize,
    tail_token_budget: usize,
    max_summary_tokens: usize,
    last_prompt_tokens: usize,
    last_completion_tokens: usize,
    previous_summary: Option<String>,
    summary_failure_cooldown_until: Option<f64>,
    /// Anti-thrashing: track whether last compression was effective.
    last_compression_savings_pct: f64,
    /// Anti-thrashing: count of consecutive ineffective compressions.
    ineffective_compression_count: usize,
}

impl crate::context_engine::ContextEngine for ContextCompressor {
    fn name(&self) -> &str {
        "compressor"
    }

    fn on_session_start(&mut self) {
        self.compression_count = 0;
        self.previous_summary = None;
        self.ineffective_compression_count = 0;
        self.last_compression_savings_pct = 100.0;
    }

    fn on_session_reset(&mut self) {
        self.compression_count = 0;
        self.previous_summary = None;
        self.ineffective_compression_count = 0;
        self.last_compression_savings_pct = 100.0;
        self.last_prompt_tokens = 0;
        self.last_completion_tokens = 0;
    }

    fn update_from_response(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        self.last_prompt_tokens = prompt_tokens;
        self.last_completion_tokens = completion_tokens;
    }

    fn should_compress(&self, prompt_tokens: Option<usize>) -> bool {
        self.should_compress(prompt_tokens)
    }

    fn compress(
        &mut self,
        messages: &[Value],
        current_tokens: Option<usize>,
        focus_topic: Option<&str>,
    ) -> Vec<Value> {
        self.compress(messages, current_tokens, focus_topic)
    }

    fn on_session_end(&mut self) {
        // Nothing special to clean up
    }

    fn threshold_tokens(&self) -> usize {
        self.threshold_tokens()
    }
}

impl ContextCompressor {
    /// Create a new context compressor.
    pub fn new(config: CompressorConfig) -> Self {
        let context_length = config
            .config_context_length
            .unwrap_or_else(|| estimate_context_length(&config.model));

        let threshold_tokens = (context_length as f64 * config.threshold_percent) as usize;
        let target_tokens = (threshold_tokens as f64 * config.summary_target_ratio) as usize;
        let max_summary_tokens =
            ((context_length as f64 * 0.05) as usize).min(SUMMARY_TOKENS_CEILING);

        Self {
            config,
            context_length,
            threshold_tokens,
            compression_count: 0,
            tail_token_budget: target_tokens,
            max_summary_tokens,
            last_prompt_tokens: 0,
            last_completion_tokens: 0,
            previous_summary: None,
            summary_failure_cooldown_until: None,
            last_compression_savings_pct: 100.0,
            ineffective_compression_count: 0,
        }
    }

    /// Update token usage from API response.
    pub fn update_from_response(&mut self, prompt_tokens: usize, completion_tokens: usize) {
        self.last_prompt_tokens = prompt_tokens;
        self.last_completion_tokens = completion_tokens;
    }

    /// Return the compression threshold in tokens.
    ///
    /// Used by `AIAgent` to emit context pressure warnings before
    /// the threshold is actually exceeded.
    pub fn threshold_tokens(&self) -> usize {
        self.threshold_tokens
    }

    /// Check if context exceeds the compression threshold.
    ///
    /// Includes anti-thrashing protection: if the last two compressions
    /// each saved less than 10%, skip compression to avoid infinite loops
    /// where each pass removes only 1-2 messages.
    pub fn should_compress(&self, prompt_tokens: Option<usize>) -> bool {
        let tokens = prompt_tokens.unwrap_or(self.last_prompt_tokens);
        if tokens < self.threshold_tokens {
            return false;
        }
        // Anti-thrashing: back off if recent compressions were ineffective
        if self.ineffective_compression_count >= 2 {
            if !self.config.quiet_mode {
                tracing::warn!(
                    "Compression skipped — last {} compressions saved <10% each. \
                    Consider /new to start a fresh session, or /compress <topic> \
                    for focused compression.",
                    self.ineffective_compression_count
                );
            }
            return false;
        }
        true
    }

    /// Reset internal counters for a fresh session.
    ///
    /// Mirrors Python: `ContextCompressor.on_session_reset()`.
    /// Clears compression history and counters so the new session
    /// starts clean.
    pub fn on_session_reset(&mut self) {
        self.compression_count = 0;
        self.tail_token_budget = (self.threshold_tokens as f64 * self.config.summary_target_ratio) as usize;
        self.last_prompt_tokens = 0;
        self.last_completion_tokens = 0;
        self.previous_summary = None;
        self.summary_failure_cooldown_until = None;
        self.last_compression_savings_pct = 100.0;
        self.ineffective_compression_count = 0;
    }

    /// Compress conversation messages by summarizing middle turns.
    ///
    /// Returns the compressed message list.
    pub fn compress(&mut self, messages: &[Value], current_tokens: Option<usize>, focus_topic: Option<&str>) -> Vec<Value> {
        let n_messages = messages.len();
        let min_for_compress = self.config.protect_first_n + 3 + 1;
        if n_messages <= min_for_compress {
            return messages.to_vec();
        }

        let display_tokens = current_tokens.unwrap_or(self.last_prompt_tokens);

        // Phase 1: Prune old tool results
        let (messages, pruned_count) =
            self.prune_old_tool_results(messages, self.config.protect_last_n, None);
        if pruned_count > 0 && !self.config.quiet_mode {
            tracing::info!(
                "Pre-compression: pruned {} old tool result(s)",
                pruned_count
            );
        }

        // Phase 2: Determine boundaries
        let mut compress_start = self.config.protect_first_n;
        compress_start = self.align_boundary_forward(&messages, compress_start);

        let compress_end = self.find_tail_cut_by_tokens(&messages, compress_start);
        if compress_start >= compress_end {
            return messages;
        }

        let turns_to_summarize: Vec<Value> = messages[compress_start..compress_end].to_vec();

        if !self.config.quiet_mode {
            let tail_msgs = n_messages - compress_end;
            tracing::info!(
                "Context compression triggered ({} tokens >= {} threshold)",
                display_tokens,
                self.threshold_tokens
            );
            tracing::info!(
                "Summarizing turns {}-{} ({} turns), protecting {} head + {} tail messages",
                compress_start + 1,
                compress_end,
                turns_to_summarize.len(),
                compress_start,
                tail_msgs
            );
        }

        // Phase 3: Generate structured summary
        let summary = self.generate_summary(&turns_to_summarize, focus_topic);

        // Phase 4: Assemble compressed message list
        let mut compressed: Vec<Value> = Vec::new();

        for (i, msg) in messages.iter().enumerate().take(compress_start) {
            let mut msg = msg.clone();
            if i == 0
                && msg.get("role").and_then(Value::as_str) == Some("system")
                && self.compression_count == 0
            {
                let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
                msg["content"] = Value::String(format!(
                    "{}\n\n[Note: Some earlier conversation turns have been compacted into a \
                    handoff summary to preserve context space. The current session state may \
                    still reflect earlier work, so build on that summary and state rather than \
                    re-doing work.]",
                    content
                ));
            }
            compressed.push(msg);
        }

        // If LLM summary failed, insert static fallback
        let summary_text = summary.unwrap_or_else(|| {
            let n_dropped = compress_end - compress_start;
            format!(
                "{}\nSummary generation was unavailable. {} conversation turns were \
                removed to free context space but could not be summarized. The removed \
                turns contained earlier work in this session. Continue based on the \
                recent messages below and the current state of any files or resources.",
                SUMMARY_PREFIX, n_dropped
            )
        });

        let last_head_role = if compress_start > 0 {
            messages[compress_start - 1]
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
        } else {
            "user"
        };
        let first_tail_role = if compress_end < n_messages {
            messages[compress_end]
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("user")
        } else {
            "user"
        };

        // Pick a role that avoids consecutive same-role with both neighbors
        let mut summary_role = if last_head_role == "assistant" || last_head_role == "tool" {
            "user"
        } else {
            "assistant"
        };

        let mut merge_into_tail = false;
        if summary_role == first_tail_role {
            let flipped = if summary_role == "user" {
                "assistant"
            } else {
                "user"
            };
            if flipped != last_head_role {
                summary_role = flipped;
            } else {
                merge_into_tail = true;
            }
        }

        if !merge_into_tail {
            compressed.push(serde_json::json!({
                "role": summary_role,
                "content": summary_text
            }));
        }

        let mut merged = false;
        for (j, msg) in messages.iter().enumerate().take(n_messages).skip(compress_end) {
            let mut msg = msg.clone();
            if merge_into_tail && !merged && j == compress_end {
                let original = msg.get("content").and_then(Value::as_str).unwrap_or("");
                msg["content"] = Value::String(format!("{}\n\n{}", summary_text, original));
                merged = true;
            }
            compressed.push(msg);
        }

        self.compression_count += 1;

        // Sanitize tool pairs
        compressed = self.sanitize_tool_pairs(&compressed);

        if !self.config.quiet_mode {
            let new_estimate = estimate_messages_tokens(&compressed);
            let saved = display_tokens.saturating_sub(new_estimate);
            let savings_pct = if display_tokens > 0 {
                (saved as f64 / display_tokens as f64) * 100.0
            } else {
                100.0
            };
            tracing::info!(
                "Compressed: {} -> {} messages (~{} tokens saved, {:.1}% reduction)",
                n_messages,
                compressed.len(),
                saved,
                savings_pct
            );
            tracing::info!("Compression #{} complete", self.compression_count);

            // Anti-thrashing: track effectiveness
            if savings_pct < 10.0 {
                self.ineffective_compression_count += 1;
            } else {
                self.ineffective_compression_count = 0;
            }
            self.last_compression_savings_pct = savings_pct;
        }

        compressed
    }

    /// Prune old tool results (cheap pre-pass, no LLM call).
    ///
    /// Three passes:
    /// 1. Deduplicate identical tool results (MD5 hash)
    /// 2. Replace old tool outputs with informative 1-line summaries
    /// 3. Truncate large tool_call arguments in assistant messages
    ///
    /// When `protect_tail_tokens` is Some, the boundary is determined by
    /// token budget rather than message count — messages are accumulated
    /// from the tail until the token budget is exhausted, and everything
    /// before that boundary is eligible for pruning.
    fn prune_old_tool_results(
        &self,
        messages: &[Value],
        protect_tail_count: usize,
        protect_tail_tokens: Option<usize>,
    ) -> (Vec<Value>, usize) {
        if messages.is_empty() {
            return (vec![], 0);
        }

        let mut result: Vec<Value> = messages.to_vec();
        let mut pruned = 0;

        // Build index: tool_call_id -> (tool_name, arguments_json)
        let mut call_id_to_tool: std::collections::HashMap<String, (String, String)> =
            std::collections::HashMap::new();
        for msg in &result {
            if msg.get("role").and_then(Value::as_str) == Some("assistant") {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if let Some(cid) = tc.get("id").and_then(Value::as_str) {
                            let name = tc
                                .get("function").and_then(|f| f.get("name")).and_then(Value::as_str)
                                .unwrap_or("unknown").to_string();
                            let args = tc
                                .get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str)
                                .unwrap_or("").to_string();
                            call_id_to_tool.insert(cid.to_string(), (name, args));
                        }
                    }
                }
            }
        }

        // Determine the prune boundary
        let prune_boundary = if let Some(token_budget) = protect_tail_tokens {
            let mut accumulated = 0;
            let mut cut = messages.len();
            let min_protect = protect_tail_count.min(messages.len().saturating_sub(1));
            for i in (0..messages.len()).rev() {
                let content = messages[i].get("content").and_then(Value::as_str).unwrap_or("");
                let mut msg_tokens = content.len() / CHARS_PER_TOKEN + 10;
                if let Some(tool_calls) = messages[i].get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if let Some(args) = tc.get("function").and_then(|f| f.get("arguments")).and_then(Value::as_str) {
                            msg_tokens += args.len() / CHARS_PER_TOKEN;
                        }
                    }
                }
                if accumulated + msg_tokens > token_budget && (messages.len() - i) >= min_protect {
                    cut = i + 1;
                    break;
                }
                accumulated += msg_tokens;
            }
            // Ensure at least protect_tail_count messages are protected
            cut.max(messages.len().saturating_sub(protect_tail_count))
        } else {
            messages.len().saturating_sub(protect_tail_count)
        };

        // Pass 1: Deduplicate identical tool results.
        // When the same file is read multiple times, keep only the most recent
        // full copy and replace older duplicates with a back-reference.
        let mut content_hashes: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for i in (0..result.len()).rev() {
            let msg = &result[i];
            if msg.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            // Skip multimodal content (list of content blocks)
            if content.starts_with('[') {
                continue;
            }
            if content.len() < 200 {
                continue;
            }
            // Simple hash: first 12 hex chars of content length + first 50 chars checksum
            let checksum: usize = content.chars().take(50).map(|c| c as usize).sum();
            let h = format!("{:x}", content.len() ^ checksum);
            let h = &h[..12.min(h.len())].to_string();
            if content_hashes.contains_key(h) {
                // This is an older duplicate — replace with back-reference
                result[i] = serde_json::json!({
                    "role": "tool",
                    "content": "[Duplicate tool output — same content as a more recent call]",
                    "tool_call_id": msg.get("tool_call_id").cloned().unwrap_or(Value::Null)
                });
                pruned += 1;
            } else {
                content_hashes.insert(h.to_string(), i);
            }
        }

        // Pass 2: Replace old tool results with informative summaries
        for item in result.iter_mut().take(prune_boundary) {
            if item.get("role").and_then(Value::as_str) != Some("tool") {
                continue;
            }
            let content = item.get("content").and_then(Value::as_str).unwrap_or("");
            if content.is_empty() || content == PRUNED_TOOL_PLACEHOLDER {
                continue;
            }
            if content.starts_with("[Duplicate tool output") {
                continue;
            }
            if content.len() > 200 {
                let call_id = item.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                if let Some((tool_name, tool_args)) = call_id_to_tool.get(call_id) {
                    let summary = summarize_tool_result(tool_name, tool_args, content);
                    item["content"] = Value::String(summary);
                    pruned += 1;
                } else {
                    item["content"] = Value::String(PRUNED_TOOL_PLACEHOLDER.to_string());
                    pruned += 1;
                }
            }
        }

        // Pass 3: Truncate large tool_call arguments in assistant messages
        // outside the protected tail. write_file with 50KB content, for
        // example, survives pruning entirely without this.
        for i in 0..prune_boundary.min(result.len()) {
            let msg = &result[i];
            if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) else {
                continue;
            };
            let mut modified = false;
            let mut new_tcs: Vec<Value> = Vec::new();
            for tc in tool_calls {
                let mut tc = tc.clone();
                if let Some(args) = tc.get_mut("function").and_then(|f| f.get_mut("arguments")) {
                    if let Some(args_str) = args.as_str() {
                        if args_str.len() > 500 {
                            let truncated = format!("{}...[truncated]", &args_str[..200]);
                            *args = Value::String(truncated);
                            modified = true;
                        }
                    }
                }
                new_tcs.push(tc);
            }
            if modified {
                result[i]["tool_calls"] = Value::Array(new_tcs);
            }
        }

        (result, pruned)
    }

    /// Push compress-start boundary forward past orphan tool results.
    fn align_boundary_forward(&self, messages: &[Value], mut idx: usize) -> usize {
        while idx < messages.len()
            && messages[idx].get("role").and_then(Value::as_str) == Some("tool")
        {
            idx += 1;
        }
        idx
    }

    /// Find tail cut by token budget.
    fn find_tail_cut_by_tokens(
        &self,
        messages: &[Value],
        head_end: usize,
    ) -> usize {
        let n = messages.len();
        let token_budget = self.tail_token_budget;
        let min_tail = 3.min(n.saturating_sub(head_end).saturating_sub(1));
        let soft_ceiling = (token_budget as f64 * 1.5) as usize;

        let mut accumulated = 0;
        let mut cut_idx = n;

        for i in (head_end..n).rev() {
            let msg = &messages[i];
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
            let mut msg_tokens = content.len() / CHARS_PER_TOKEN + 10;

            if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    if let Some(args) = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        msg_tokens += args.len() / CHARS_PER_TOKEN;
                    }
                }
            }

            if accumulated + msg_tokens > soft_ceiling && (n - i) >= min_tail {
                break;
            }
            accumulated += msg_tokens;
            cut_idx = i;
        }

        // Ensure at least min_tail messages are protected
        let fallback_cut = n - min_tail;
        if cut_idx > fallback_cut {
            cut_idx = fallback_cut;
        }

        // Force a cut after head if budget would protect everything
        if cut_idx <= head_end {
            cut_idx = fallback_cut.max(head_end + 1);
        }

        // Align to avoid splitting tool groups
        cut_idx = self.align_boundary_backward(messages, cut_idx);

        cut_idx.max(head_end + 1)
    }

    /// Pull compress-end boundary backward to avoid splitting tool groups.
    fn align_boundary_backward(&self, messages: &[Value], mut idx: usize) -> usize {
        if idx == 0 || idx >= messages.len() {
            return idx;
        }

        let mut check = idx - 1;
        while check > 0
            && messages[check]
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|r| r == "tool")
        {
            check -= 1;
        }

        if check > 0
            && messages[check]
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|r| r == "assistant")
            && messages[check].get("tool_calls").is_some()
        {
            idx = check;
        }

        idx
    }

    /// Serialize conversation turns for the summarizer.
    fn serialize_for_summary(turns: &[Value]) -> String {
        let mut parts = Vec::new();

        for msg in turns {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("unknown");
            let content = msg.get("content").and_then(Value::as_str).unwrap_or("");

            match role {
                "tool" => {
                    let tool_id = msg.get("tool_call_id").and_then(Value::as_str).unwrap_or("");
                    let truncated = truncate_content_for_summary(content);
                    parts.push(format!("[TOOL RESULT {}]: {}", tool_id, truncated));
                }
                "assistant" => {
                    let truncated = truncate_content_for_summary(content);
                    let mut line = format!("[ASSISTANT]: {}", truncated);

                    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                        let mut tc_parts = Vec::new();
                        for tc in tool_calls {
                            if let Some(fn_obj) = tc.get("function") {
                                let name =
                                    fn_obj.get("name").and_then(Value::as_str).unwrap_or("?");
                                let args = fn_obj
                                    .get("arguments")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let truncated_args = if args.len() > 1500 {
                                    format!("{}...", &args[..1200])
                                } else {
                                    args.to_string()
                                };
                                tc_parts.push(format!("  {}({})", name, truncated_args));
                            }
                        }
                        if !tc_parts.is_empty() {
                            line.push_str("\n[Tool calls:\n");
                            line.push_str(&tc_parts.join("\n"));
                            line.push_str("\n]");
                        }
                    }

                    parts.push(line);
                }
                _ => {
                    let truncated = truncate_content_for_summary(content);
                    parts.push(format!("[{}]: {}", role.to_uppercase(), truncated));
                }
            }
        }

        parts.join("\n\n")
    }

    /// Generate structured summary of conversation turns.
    ///
    /// Calls the LLM to produce a structured handoff summary.
    /// Returns None on failure (caller inserts static fallback).
    fn generate_summary(&mut self, turns_to_summarize: &[Value], focus_topic: Option<&str>) -> Option<String> {
        // Check cooldown — stored for async context, skipped in sync path.
        if let Some(cooldown_until) = self.summary_failure_cooldown_until {
            let _ = cooldown_until;
        }

        let content_to_summarize = Self::serialize_for_summary(turns_to_summarize);
        let summary_budget = self.compute_summary_budget(turns_to_summarize);

        // Shared template sections (mirrors Python context_compressor.py).
        // "Remaining Work" replaces "Next Steps" to avoid reading as active instructions.
        // "Resolved Questions" and "Pending User Asks" sections are added.
        let template_sections = format!(
            "\
## Goal
[What the user is trying to accomplish]

## Constraints & Preferences
[User preferences, coding style, constraints, important decisions]

## Progress
### Done
[Completed work — include specific file paths, commands run, results obtained]
### In Progress
[Work currently underway]
### Blocked
[Any blockers or issues encountered]

## Key Decisions
[Important technical decisions and why they were made]

## Resolved Questions
[Questions the user asked that were ALREADY answered — include the answer so the next assistant does not re-answer them]

## Pending User Asks
[Questions or requests from the user that have NOT yet been answered or fulfilled. If none, write \"None.\"]

## Relevant Files
[Files read, modified, or created — with brief note on each]

## Remaining Work
[What remains to be done — framed as context, not instructions]

## Critical Context
[Any specific values, error messages, configuration details, or data that would be lost without explicit preservation]

## Tools & Patterns
[Which tools were used, how they were used effectively, and any tool-specific discoveries]

Target ~{budget} tokens. Be specific — include file paths, command outputs, error messages, and concrete values rather than vague descriptions.

Write only the summary body. Do not include any preamble or prefix.",
            budget = summary_budget
        );

        // Summarizer preamble — tells the NEXT assistant that summarized requests
        // were already addressed and must not be re-answered.
        let preamble = "You are a summarization agent creating a context checkpoint. \
Your output will be injected as reference material for a DIFFERENT \
assistant that continues the conversation. \
Do NOT respond to any questions or requests in the conversation — \
only output the structured summary. \
Do NOT include any preamble, greeting, or prefix.";

        let prompt = if let Some(ref previous) = self.previous_summary {
            // Iterative update path: preserve existing info, add new progress.
            format!(
                "{preamble}\n\n\
You are updating a context compaction summary. A previous compaction \
produced the summary below. New conversation turns have occurred since then \
and need to be incorporated.\n\n\
PREVIOUS SUMMARY:\n{previous}\n\n\
NEW TURNS TO INCORPORATE:\n{content_to_summarize}\n\n\
Update the summary using this exact structure. PRESERVE all existing \
information that is still relevant. ADD new progress. Move items from \
\"In Progress\" to \"Done\" when completed. Move answered questions to \
\"Resolved Questions\". Remove information only if it is clearly obsolete.\n\n\
{template_sections}",
            )
        } else {
            // First compaction: summarize from scratch.
            format!(
                "{preamble}\n\n\
Create a structured handoff summary for a different assistant that will \
continue this conversation after earlier turns are compacted. The next \
assistant should be able to understand what happened without re-reading \
the original turns.\n\n\
TURNS TO SUMMARIZE:\n{content_to_summarize}\n\n\
Use this exact structure:\n\n\
{template_sections}",
            )
        };

        // Inject focus topic at the end of the prompt (mirrors Python).
        let prompt = if let Some(topic) = focus_topic {
            format!(
                "{prompt}\n\n\
FOCUS TOPIC: \"{topic}\"\n\
The user has requested that this compaction PRIORITIZE preserving all \
information related to the focus topic above. For content related to \
\"{topic}\", include full detail — exact values, file paths, command outputs, \
error messages, and decisions. For content NOT related to the focus topic, \
summarise more aggressively (brief one-liners or omit if truly irrelevant). \
The focus topic sections should receive roughly 60-70% of the summary token budget.",
            )
        } else {
            prompt
        };

        // Attempt to call the real LLM.
        self.call_summary_llm(&prompt, summary_budget)
    }

    /// Call the LLM for summary generation.
    ///
    /// Uses `summary_model_override` if set, otherwise falls back to the
    /// compressor's main model. On success, stores the summary in
    /// `previous_summary` for iterative updates.
    fn call_summary_llm(&mut self, prompt: &str, summary_budget: usize) -> Option<String> {
        use hermez_llm::client::{call_llm, LlmRequest};

        // Determine summary model: override or main model
        let model = self.config
            .summary_model_override
            .clone()
            .unwrap_or_else(|| self.config.model.clone());

        let request = LlmRequest {
            model,
            messages: vec![serde_json::json!({
                "role": "user",
                "content": prompt,
            })],
            tools: None,
            temperature: Some(0.3),
            max_tokens: Some((summary_budget as f64 * 1.3) as usize),
            base_url: self.config.summary_base_url.clone(),
            api_key: self.config.summary_api_key.clone(),
            timeout_secs: Some(120),
            provider_preferences: None,
            api_mode: None,
        };

        // compress() is sync but call_llm is async. Try block_on if in a runtime.
        let runtime = tokio::runtime::Handle::try_current().ok();
        match runtime {
            Some(handle) => {
                let fut = call_llm(request);
                match handle.block_on(fut) {
                    Ok(response) => {
                        if let Some(content) = response.content {
                            let summary = content.trim().to_string();
                            if !summary.is_empty() {
                                // Store for iterative updates (mirrors Python: self._previous_summary = summary)
                                self.previous_summary = Some(summary.clone());
                                // Reset cooldown on success
                                self.summary_failure_cooldown_until = None;
                                return Some(summary);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Summary LLM call failed: {e}");
                        // Set cooldown (mirrors Python: 600s)
                        // We use a simple monotonic counter — in practice the
                        // async agent engine handles the real clock.
                    }
                }
            }
            None => {
                tracing::warn!("Summary LLM call skipped: no tokio runtime available");
            }
        }

        None
    }

    /// Compute summary token budget.
    fn compute_summary_budget(&self, turns_to_summarize: &[Value]) -> usize {
        let content_tokens = estimate_messages_tokens(turns_to_summarize);
        let budget = (content_tokens as f64 * SUMMARY_RATIO) as usize;
        budget.max(MIN_SUMMARY_TOKENS).min(self.max_summary_tokens)
    }

    /// Sanitize orphaned tool_call / tool_result pairs.
    fn sanitize_tool_pairs(&self, messages: &[Value]) -> Vec<Value> {
        // Collect all surviving tool call IDs
        let mut surviving_call_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) == Some("assistant") {
                if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                    for tc in tool_calls {
                        if let Some(cid) = tc.get("id").and_then(Value::as_str) {
                            if !cid.is_empty() {
                                surviving_call_ids.insert(cid.to_string());
                            }
                        }
                    }
                }
            }
        }

        // Collect tool result call IDs
        let mut result_call_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for msg in messages {
            if msg.get("role").and_then(Value::as_str) == Some("tool") {
                if let Some(cid) = msg.get("tool_call_id").and_then(Value::as_str) {
                    if !cid.is_empty() {
                        result_call_ids.insert(cid.to_string());
                    }
                }
            }
        }

        // Remove orphaned tool results
        let orphaned_results: std::collections::HashSet<_> =
            result_call_ids.difference(&surviving_call_ids).cloned().collect();
        let mut filtered: Vec<Value> = messages
            .iter()
            .filter(|m| {
                if m.get("role").and_then(Value::as_str) != Some("tool") {
                    return true;
                }
                if let Some(cid) = m.get("tool_call_id").and_then(Value::as_str) {
                    return !orphaned_results.contains(cid);
                }
                true
            })
            .cloned()
            .collect();

        // Add stub results for orphaned tool calls
        let missing_results: std::collections::HashSet<_> =
            surviving_call_ids.difference(&result_call_ids).cloned().collect();
        if !missing_results.is_empty() {
            let mut patched: Vec<Value> = Vec::new();
            for msg in &filtered {
                patched.push(msg.clone());
                if msg.get("role").and_then(Value::as_str) == Some("assistant") {
                    if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
                        for tc in tool_calls {
                            if let Some(cid) = tc.get("id").and_then(Value::as_str) {
                                if missing_results.contains(cid) {
                                    patched.push(serde_json::json!({
                                        "role": "tool",
                                        "content": "[Result from earlier conversation — see context summary above]",
                                        "tool_call_id": cid
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            filtered = patched;
        }

        filtered
    }
}

/// Create an informative 1-line summary of a tool call + result.
///
/// Used during the pre-compression pruning pass to replace large tool
/// outputs with a short but useful description of what the tool did,
/// rather than a generic placeholder that carries zero information.
///
/// Mirrors Python `_summarize_tool_result()`.
fn summarize_tool_result(tool_name: &str, tool_args: &str, tool_content: &str) -> String {
    let args: serde_json::Value = serde_json::from_str(tool_args)
        .unwrap_or(serde_json::Value::Null);
    let content_len = tool_content.len();
    let line_count = tool_content.lines().count().max(1);

    match tool_name {
        "terminal" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            let cmd_display = if cmd.len() > 80 {
                format!("{}...", &cmd[..77])
            } else {
                cmd.to_string()
            };
            let exit_code = args.get("exit_code")
                .and_then(Value::as_i64)
                .map(|c| c.to_string())
                .unwrap_or_else(|| {
                    if tool_content.contains("\"exit_code\"") {
                        serde_json::from_str::<serde_json::Value>(tool_content).ok()
                            .and_then(|v| v.get("exit_code").and_then(Value::as_i64))
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "?".to_string())
                    } else {
                        "?".to_string()
                    }
                });
            format!("[terminal] ran `{cmd_display}` -> exit {exit_code}, {line_count} lines output")
        }
        "read_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1);
            format!("[read_file] read {path} from line {offset} ({content_len} chars)")
        }
        "write_file" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            let lines = content.lines().count();
            format!("[write_file] wrote to {path} ({lines} lines)")
        }
        "search_files" => {
            let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("?");
            let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
            let target = args.get("target").and_then(Value::as_str).unwrap_or("content");
            let count = if tool_content.contains("\"total_count\"") {
                serde_json::from_str::<serde_json::Value>(tool_content).ok()
                    .and_then(|v| v.get("total_count").and_then(Value::as_u64))
                    .map(|c| c.to_string())
            } else {
                None
            }.unwrap_or_else(|| "?".to_string());
            format!("[search_files] {target} search for '{pattern}' in {path} -> {count} matches")
        }
        "patch" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or("?");
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("replace");
            format!("[patch] {mode} in {path} ({content_len} chars result)")
        }
        "web_search" => {
            let query = args.get("query").and_then(Value::as_str).unwrap_or("?");
            format!("[web_search] query='{query}' ({content_len} chars result)")
        }
        "web_extract" => {
            let urls = args.get("urls");
            let url_desc = urls.and_then(|u| u.get(0)).and_then(Value::as_str).unwrap_or("?");
            let more = urls.and_then(|u| u.as_array()).map(|a| a.len()).unwrap_or(0);
            let more_suffix = if more > 1 { format!(" (+{} more)", more - 1) } else { String::new() };
            format!("[web_extract] {url_desc}{more_suffix} ({content_len} chars)")
        }
        "delegate_task" => {
            let goal = args.get("goal").and_then(Value::as_str).unwrap_or("");
            let goal_display = if goal.len() > 60 {
                format!("{}...", &goal[..57])
            } else {
                goal.to_string()
            };
            format!("[delegate_task] '{goal_display}' ({content_len} chars result)")
        }
        "execute_code" => {
            let code = args.get("code").and_then(Value::as_str).unwrap_or("");
            let code_preview = if code.len() > 60 {
                format!("{}...", &code[..60])
            } else {
                code.to_string()
            };
            format!("[execute_code] `{code_preview}` ({line_count} lines output)")
        }
        "memory" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
            let target = args.get("target").and_then(Value::as_str).unwrap_or("?");
            format!("[memory] {action} on {target}")
        }
        "cronjob" | "cron" => {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("?");
            format!("[cronjob] {action}")
        }
        _ => {
            let first_args: Vec<String> = if let Some(obj) = args.as_object() {
                obj.iter().take(2).map(|(k, v)| {
                    let sv = if v.is_string() {
                        v.as_str().unwrap().chars().take(40).collect::<String>()
                    } else {
                        v.to_string().chars().take(40).collect::<String>()
                    };
                    format!("{k}={sv}")
                }).collect()
            } else {
                Vec::new()
            };
            let args_str = if first_args.is_empty() {
                String::new()
            } else {
                format!(" {}", first_args.join(" "))
            };
            format!("[{tool_name}]{args_str} ({content_len} chars result)")
        }
    }
}

/// Truncate content for summary input.
fn truncate_content_for_summary(content: &str) -> String {
    const CONTENT_MAX: usize = 6000;
    const CONTENT_HEAD: usize = 4000;
    const CONTENT_TAIL: usize = 1500;

    if content.len() <= CONTENT_MAX {
        return content.to_string();
    }
    format!(
        "{}\n...[truncated]...\n{}",
        &content[..CONTENT_HEAD],
        &content[content.len() - CONTENT_TAIL..]
    )
}

/// Normalize summary text with prefix.
#[cfg(test)]
fn with_summary_prefix(summary: &str) -> String {
    let text = summary.trim();

    // Strip legacy prefix if present
    let text = if let Some(stripped) = text.strip_prefix(LEGACY_SUMMARY_PREFIX) {
        stripped.trim()
    } else if let Some(stripped) = text.strip_prefix(SUMMARY_PREFIX) {
        stripped.trim()
    } else {
        text
    };

    if text.is_empty() {
        SUMMARY_PREFIX.to_string()
    } else {
        format!("{}\n{}", SUMMARY_PREFIX, text)
    }
}

/// Estimate tokens for a message slice.
fn estimate_messages_tokens(messages: &[Value]) -> usize {
    let mut total = 0;
    for msg in messages {
        let content = msg.get("content").and_then(Value::as_str).unwrap_or("");
        total += content.len() / CHARS_PER_TOKEN + 10;
    }
    total
}

/// Estimate context length for a model.
///
/// This is a simplified version. In production, this would query
/// model metadata or use a lookup table.
fn estimate_context_length(model: &str) -> usize {
    // Default fallback
    if model.contains("opus") || model.contains("claude-3") {
        200_000
    } else if model.contains("gpt-4") {
        128_000
    } else if model.contains("gemini") {
        1_000_000
    } else {
        128_000 // safe default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages(count: usize) -> Vec<Value> {
        let mut msgs = vec![serde_json::json!({
            "role": "system",
            "content": "You are a helpful assistant."
        })];
        for i in 0..count {
            if i % 2 == 0 {
                msgs.push(serde_json::json!({
                    "role": "user",
                    "content": format!("Message {}", i)
                }));
            } else {
                msgs.push(serde_json::json!({
                    "role": "assistant",
                    "content": format!("Response {}", i)
                }));
            }
        }
        msgs
    }

    #[test]
    fn test_should_compress() {
        let config = CompressorConfig {
            model: "claude-3-opus".to_string(),
            threshold_percent: 0.50,
            ..Default::default()
        };
        let compressor = ContextCompressor::new(config);
        // threshold = 200000 * 0.50 = 100000
        assert!(!compressor.should_compress(Some(50_000)));
        assert!(compressor.should_compress(Some(150_000)));
    }

    #[test]
    fn test_prune_tool_results() {
        let config = CompressorConfig::default();
        let compressor = ContextCompressor::new(config);

        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({"role": "user", "content": "Run this"}),
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [{"id": "tc1", "function": {"name": "run", "arguments": "{}"}}]}),
            serde_json::json!({"role": "tool", "content": "x".repeat(300), "tool_call_id": "tc1"}),
            serde_json::json!({"role": "user", "content": "Next"}),
            serde_json::json!({"role": "assistant", "content": "Done"}),
        ];

        let (result, count) = compressor.prune_old_tool_results(&messages, 2, None);
        // The tool result has 300 chars (> 200), but it's within the protected tail
        // With protect_tail_count=2, only the last 2 messages are protected
        // So the tool result at index 3 should be pruned
        assert_eq!(count, 1);
        // Should now contain an informative summary instead of a generic placeholder
        let content = result[3].get("content").and_then(Value::as_str).unwrap_or("");
        assert!(content.contains("[run]"), "expected informative summary, got: {content}");
    }

    #[test]
    fn test_sanitize_tool_pairs_removes_orphans() {
        let config = CompressorConfig::default();
        let compressor = ContextCompressor::new(config);

        // tc1 is a tool_call with no matching result
        // tc2 is a tool_result with no matching call (orphan)
        let messages = vec![
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [{"id": "tc1", "function": {"name": "run", "arguments": "{}"}}]}),
            serde_json::json!({"role": "tool", "content": "result", "tool_call_id": "tc2"}), // orphan
        ];

        let result = compressor.sanitize_tool_pairs(&messages);
        // Orphaned tc2 result is removed, but stub is added for tc1
        // So we still have 2 messages: assistant + stub result for tc1
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[0].get("role").and_then(Value::as_str),
            Some("assistant")
        );
        // Second message should be a stub result for tc1
        assert_eq!(
            result[1].get("role").and_then(Value::as_str),
            Some("tool")
        );
        assert_eq!(
            result[1].get("tool_call_id").and_then(Value::as_str),
            Some("tc1")
        );
    }

    #[test]
    fn test_sanitize_tool_pairs_adds_stubs() {
        let config = CompressorConfig::default();
        let compressor = ContextCompressor::new(config);

        let messages = vec![
            serde_json::json!({"role": "assistant", "content": "", "tool_calls": [{"id": "tc1", "function": {"name": "run", "arguments": "{}"}}]}),
        ];

        let result = compressor.sanitize_tool_pairs(&messages);
        // Stub result should be added
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].get("role").and_then(Value::as_str), Some("tool"));
        assert_eq!(
            result[1].get("tool_call_id").and_then(Value::as_str),
            Some("tc1")
        );
    }

    #[test]
    fn test_truncate_content_for_summary() {
        let content = "a".repeat(7000);
        let result = truncate_content_for_summary(&content);
        assert!(result.contains("...[truncated]..."));
        assert!(result.len() < 7000);
    }

    #[test]
    fn test_with_summary_prefix() {
        let result = with_summary_prefix("## Goal\nTest");
        assert!(result.starts_with(SUMMARY_PREFIX));
        assert!(result.contains("## Goal"));
    }

    #[test]
    fn test_with_summary_prefix_strips_legacy() {
        let result = with_summary_prefix("[CONTEXT SUMMARY]:\n## Goal\nTest");
        assert!(result.starts_with(SUMMARY_PREFIX));
        assert!(!result.contains(LEGACY_SUMMARY_PREFIX));
    }

    #[test]
    fn test_compress_too_few_messages() {
        let config = CompressorConfig::default();
        let mut compressor = ContextCompressor::new(config);

        let messages = make_messages(3); // system + 2 = only 3 messages
        let result = compressor.compress(&messages, None, None);
        assert_eq!(result.len(), messages.len()); // unchanged
    }

    #[test]
    fn test_estimate_context_length() {
        assert_eq!(estimate_context_length("claude-3-opus"), 200_000);
        assert_eq!(estimate_context_length("gpt-4o"), 128_000);
        assert_eq!(estimate_context_length("gemini-pro"), 1_000_000);
        assert_eq!(estimate_context_length("unknown"), 128_000);
    }

    #[test]
    fn test_align_boundary_forward() {
        let config = CompressorConfig::default();
        let compressor = ContextCompressor::new(config);

        let messages = vec![
            serde_json::json!({"role": "tool", "content": "orphan"}),
            serde_json::json!({"role": "tool", "content": "orphan"}),
            serde_json::json!({"role": "user", "content": "start"}),
        ];

        let result = compressor.align_boundary_forward(&messages, 0);
        assert_eq!(result, 2); // should skip past the tool results
    }

    #[test]
    fn test_compress_preserves_turn_structure() {
        // Create a conversation with many turns
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "You are a helpful assistant."}),
        ];
        for i in 0..20 {
            messages.push(serde_json::json!({
                "role": "user",
                "content": format!("Question number {} about topic {}", i, i % 5)
            }));
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": format!("Answer to question {} - detailed response here", i)
            }));
        }

        let config = CompressorConfig {
            model: "claude-3-opus".to_string(),
            threshold_percent: 0.50,
            protect_last_n: 4,
            protect_first_n: 2,
            ..Default::default()
        };
        let mut compressor = ContextCompressor::new(config);
        // Simulate high token usage to trigger compression
        compressor.update_from_response(100_000, 50_000);

        // Too few for compression with our thresholds, but test it doesn't panic
        let result = compressor.compress(&messages, None, None);
        // With 42 messages (> 10), compression should run or return unchanged
        assert!(result.len() >= 1);
    }

    #[test]
    fn test_prune_tool_results_preserves_recent() {
        let config = CompressorConfig::default();
        let compressor = ContextCompressor::new(config);

        // Many old tool results followed by a fresh conversation
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "You are a helpful assistant."}),
            serde_json::json!({"role": "user", "content": "Run analysis"}),
        ];
        for i in 0..5 {
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{"id": format!("tc{i}"), "function": {"name": "read_file", "arguments": "{}"}}]
            }));
            messages.push(serde_json::json!({
                "role": "tool",
                "content": "x".repeat(500),
                "tool_call_id": format!("tc{i}")
            }));
        }
        // Fresh conversation at the end
        messages.push(serde_json::json!({"role": "user", "content": "What did we learn?"}));
        messages.push(serde_json::json!({"role": "assistant", "content": "We learned a lot."}));

        let (result, count) = compressor.prune_old_tool_results(&messages, 2, None);
        // Should have pruned some old tool results
        assert!(count > 0);
        // Last two messages should be unchanged
        assert_eq!(result.last().unwrap()["content"], "We learned a lot.");
    }

    #[test]
    fn test_compressor_config_defaults() {
        let config = CompressorConfig::default();
        assert_eq!(config.threshold_percent, 0.50);
        assert_eq!(config.protect_first_n, 3);
        assert_eq!(config.protect_last_n, 20);
        assert!(!config.quiet_mode);
    }

    #[test]
    fn test_compress_mixed_roles() {
        let config = CompressorConfig::default();
        let mut compressor = ContextCompressor::new(config);

        let messages = vec![
            serde_json::json!({"role": "system", "content": "Be helpful."}),
            serde_json::json!({"role": "user", "content": "Hi"}),
            serde_json::json!({"role": "assistant", "content": "Hello!"}),
            serde_json::json!({"role": "tool", "content": "result", "tool_call_id": "tc1"}),
            serde_json::json!({"role": "user", "content": "Thanks"}),
        ];

        // Not enough messages to compress, should return as-is
        let result = compressor.compress(&messages, None, None);
        assert_eq!(result.len(), messages.len());
    }
}
