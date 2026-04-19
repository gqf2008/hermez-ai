#![allow(dead_code)]
//! Background review agent for self-evolution.
//!
//! A lightweight AIAgent that reviews the just-completed conversation
//! and creates/updates memory entries or skills if warranted.
//! Fire-and-forget — never blocks the main conversation.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::{AIAgent, AgentConfig};
use crate::agent::types::Message;
use hermes_core::Result;
use hermes_tools::registry::ToolRegistry;

/// Run a background review of the conversation.
///
/// This is a fire-and-forget task spawned by the main agent at turn end.
/// It shares the same tool registry (and thus the same memory/skill files),
/// so any changes it makes are immediately visible to the next turn.
pub async fn run_review(
    config: AgentConfig,
    registry: Arc<ToolRegistry>,
    messages: Vec<Message>,
    review_prompt: String,
    review_memory: bool,
    review_skills: bool,
) -> Result<()> {
    tracing::info!(
        "Self-evolution: starting review (memory={}, skills={})",
        review_memory, review_skills
    );

    // Cap iterations to prevent runaway review agents
    let mut review_config = config;
    review_config.max_iterations = 8;

    let mut review_agent = AIAgent::new(review_config, registry)?;

    // Extract user message for context (truncated to avoid wasting tokens)
    const MAX_USER_MSG_LEN: usize = 2000;
    let user_msg = extract_user_message(&messages)
        .unwrap_or_default();
    let user_msg_truncated = if user_msg.len() > MAX_USER_MSG_LEN {
        format!("{}... [truncated]", user_msg.chars().take(MAX_USER_MSG_LEN).collect::<String>())
    } else {
        user_msg
    };
    let tool_call_count = count_tool_calls(&messages);

    let review_input = format!(
        "Review the following conversation turn and self-improve.\n\n\
         User message: {}\n\
         Assistant response tool calls: {}\n\n\
         {}",
        user_msg_truncated, tool_call_count, review_prompt,
    );

    let result = review_agent
        .run_conversation(&review_input, None, None)
        .await;

    // Scan for actions taken
    let actions = scan_review_actions(&result.messages);

    if actions.is_empty() {
        tracing::info!("Self-evolution: review completed, no changes made");
    } else {
        tracing::info!(
            "Self-evolution: review completed with {} change(s): {}",
            actions.len(),
            actions.join(" | ")
        );
    }

    // Clean up review agent resources
    review_agent.close();

    Ok(())
}

/// Extract the first user message from conversation history.
fn extract_user_message(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .and_then(|m| m.get("content").and_then(|v| v.as_str()))
        .map(String::from)
}

/// Count tool calls in conversation history.
fn count_tool_calls(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("tool"))
        .count()
}

/// Scan review conversation for memory/skill actions.
fn scan_review_actions(messages: &[Message]) -> Vec<String> {
    let mut actions = Vec::new();
    for msg in messages {
        if msg.get("role").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let content = msg.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if let Ok(val) = serde_json::from_str::<Value>(content) {
            let success = val.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
            if !success {
                continue;
            }
            let message = val.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let target = val.get("target").and_then(|v| v.as_str()).unwrap_or("");

            let is_created = message.contains("created");
            let is_updated = message.contains("updated");
            if is_created || is_updated {
                let label = if target.is_empty() {
                    "skill".to_string()
                } else if target == "memory" {
                    "Memory".to_string()
                } else if target == "user" {
                    "User profile".to_string()
                } else {
                    target.to_string()
                };
                if is_created {
                    actions.push(format!("{label} created"));
                } else {
                    actions.push(format!("{label} updated"));
                }
            }
        }
    }
    actions
}
