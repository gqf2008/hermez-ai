//! Message loop — platform-facing conversation coordinator.
//!
//! This module bridges external platforms (CLI stdin, gateway messages,
//! ACP requests) to the `AIAgent` conversation engine.
//!
//! Responsibilities:
//! - Maintain conversation state (message history, session ID)
//! - Handle platform-specific message formatting
//! - Support multi-turn conversations
//! - Manage session persistence
//! - Handle interrupt signals

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::Mutex as AsyncMutex;

use serde::{Deserialize, Serialize};

use hermes_core::Result;
use hermes_state::SessionDB;

use crate::agent::AIAgent;
use crate::agent::types::{ExitReason, Message};

/// A message from a platform/user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformMessage {
    /// Unique message ID (from platform).
    pub id: String,
    /// Sender ID (user ID, platform session, etc.).
    pub sender: String,
    /// Message content (text).
    pub content: String,
    /// Optional system message override.
    pub system_message: Option<String>,
    /// Timestamp (Unix epoch seconds).
    pub timestamp: u64,
}

/// Result of processing a platform message.
#[derive(Debug, Clone, Serialize)]
pub struct MessageResult {
    /// Response text to send back to the platform.
    pub response: String,
    /// Whether the agent is waiting for user input.
    pub waiting_for_user: bool,
    /// Agent metadata for the platform.
    pub metadata: MessageMetadata,
}

/// Metadata about the agent state for platform display.
#[derive(Debug, Clone, Serialize)]
pub struct MessageMetadata {
    pub session_id: String,
    pub message_count: usize,
    pub api_calls: usize,
    pub budget_remaining: usize,
    pub exit_reason: ExitReason,
}

/// Manages a conversation session across multiple platform messages.
pub struct MessageLoop {
    /// The AI agent running the conversation.
    agent: Arc<AsyncMutex<AIAgent>>,
    /// Conversation history.
    messages: Vec<Message>,
    /// Session ID for persistence.
    session_id: String,
    /// Session database.
    session_db: Option<Arc<SessionDB>>,
    /// Shared interrupt flag.
    interrupt: Arc<AtomicBool>,
    /// Number of messages already persisted to DB.
    persisted_count: usize,
}

impl MessageLoop {
    /// Create a new message loop with the given agent.
    pub fn new(
        agent: AIAgent,
        session_id: String,
        session_db: Option<Arc<SessionDB>>,
        interrupt: Arc<AtomicBool>,
    ) -> Self {
        Self {
            agent: Arc::new(AsyncMutex::new(agent)),
            messages: Vec::new(),
            session_id,
            session_db,
            interrupt,
            persisted_count: 0,
        }
    }

    /// Process a single incoming message and return the response.
    pub async fn process_message(&mut self, msg: PlatformMessage) -> Result<MessageResult> {
        // Check for interrupt
        if self.interrupt.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(MessageResult {
                response: "Conversation interrupted.".to_string(),
                waiting_for_user: true,
                metadata: MessageMetadata {
                    session_id: self.session_id.clone(),
                    message_count: self.messages.len(),
                    api_calls: 0,
                    budget_remaining: 0,
                    exit_reason: ExitReason::Interrupted,
                },
            });
        }

        let system_msg = msg.system_message.as_deref();

        // Run the conversation through the agent
        let turn_result = {
            let mut agent = self.agent.lock().await;
            agent.run_conversation(&msg.content, system_msg, Some(&self.messages)).await
        };

        // Update message history
        self.messages = turn_result.messages.clone();

        // Persist new messages to session DB (only append the latest ones)
        if let Some(ref db) = self.session_db {
            let start = self.persisted_count;
            for msg in &self.messages[start..] {
                if let (Some(role), Some(content)) = (
                    msg.get("role").and_then(|v| v.as_str()),
                    msg.get("content").and_then(|v| v.as_str()),
                ) {
                    if let Err(e) = db.append_message(&self.session_id, role, Some(content),
                        None, None, None, None, None, None, None, None) {
                        tracing::warn!("Failed to append message to session DB: {e}");
                    }
                }
            }
            self.persisted_count = self.messages.len();
        }

        let budget_remaining = {
            let agent = self.agent.lock().await;
            agent.budget.remaining()
        };

        Ok(MessageResult {
            response: turn_result.response.clone(),
            waiting_for_user: turn_result.exit_reason == ExitReason::Completed,
            metadata: MessageMetadata {
                session_id: self.session_id.clone(),
                message_count: self.messages.len(),
                api_calls: turn_result.api_calls,
                budget_remaining,
                exit_reason: turn_result.exit_reason.clone(),
            },
        })
    }

    /// Get the current conversation history.
    pub fn history(&self) -> &[Message] {
        &self.messages
    }

    /// Clear conversation history.
    pub fn clear_history(&mut self) {
        self.messages.clear();
    }

    /// Get the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Check if the conversation was interrupted.
    pub fn is_interrupted(&self) -> bool {
        self.interrupt.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Signal an interrupt (stops the current turn).
    pub fn interrupt(&self) {
        self.interrupt.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reset the interrupt flag for a new conversation.
    pub fn reset_interrupt(&self) {
        self.interrupt.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_loop_creation() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let config = crate::agent::AgentConfig::default();
        let registry = Arc::new(hermes_tools::registry::ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        let loop_ = MessageLoop::new(agent, "test-session".to_string(), None, interrupt.clone());

        assert_eq!(loop_.session_id(), "test-session");
        assert!(loop_.history().is_empty());
        assert!(!loop_.is_interrupted());
    }

    #[test]
    fn test_interrupt_flow() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let config = crate::agent::AgentConfig::default();
        let registry = Arc::new(hermes_tools::registry::ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        let loop_ = MessageLoop::new(agent, "test".to_string(), None, interrupt.clone());

        assert!(!loop_.is_interrupted());
        loop_.interrupt();
        assert!(loop_.is_interrupted());
        loop_.reset_interrupt();
        assert!(!loop_.is_interrupted());
    }

    #[test]
    fn test_clear_history() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let config = crate::agent::AgentConfig::default();
        let registry = Arc::new(hermes_tools::registry::ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        let mut loop_ = MessageLoop::new(agent, "test".to_string(), None, interrupt);

        assert!(loop_.history().is_empty());
        loop_.messages.push(Arc::new(serde_json::json!({"role": "user", "content": "hello"})));
        assert_eq!(loop_.history().len(), 1);
        loop_.clear_history();
        assert!(loop_.history().is_empty());
    }

    #[tokio::test]
    async fn test_interrupted_message_returns_early() {
        let interrupt = Arc::new(AtomicBool::new(true)); // Start interrupted
        let config = crate::agent::AgentConfig::default();
        let registry = Arc::new(hermes_tools::registry::ToolRegistry::new());
        let agent = AIAgent::new(config, registry).unwrap();
        let mut loop_ = MessageLoop::new(agent, "test".to_string(), None, interrupt);

        let msg = PlatformMessage {
            id: "1".to_string(),
            sender: "user".to_string(),
            content: "hello".to_string(),
            system_message: None,
            timestamp: 0,
        };
        let result = loop_.process_message(msg).await.unwrap();
        assert_eq!(result.metadata.exit_reason, ExitReason::Interrupted);
        assert_eq!(result.response, "Conversation interrupted.");
    }
}
