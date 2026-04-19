//! ACP server — JSON-RPC dispatch over stdin/stdout.
//!
//! Implements the agent-client-protocol server endpoint.
//! Mirrors `acp_adapter/server.py` — `HermesACPAgent` class.

#![allow(dead_code)]

use hermes_agent_engine::agent::{AgentConfig, AIAgent};
use hermes_core::HermesConfig;
use hermes_tools::registry::ToolRegistry;
use std::sync::Arc;

use crate::protocol::*;
use crate::session::{SessionManager, SessionState};

/// ACP server implementation.
pub struct AcpServer {
    session_manager: Arc<SessionManager>,
    /// Channel for sending session updates to the client.
    /// Uses a bounded channel to avoid blocking the agent.
    update_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    /// Request counter for notifications.
    request_id: std::sync::atomic::AtomicU64,
}

// ---- Tool kind mapping (mirrors acp_adapter/tools.py TOOL_KIND_MAP) ----------

fn get_tool_kind(tool_name: &str) -> &str {
    match tool_name {
        "read_file" | "search_files" => "read",
        "write_file" | "patch" => "edit",
        "terminal" | "process" => "execute",
        "web_search" | "web_extract" => "web",
        "vision_analyze" => "vision",
        "image_generate" => "generate",
        "browser_navigate"
        | "browser_snapshot"
        | "browser_click"
        | "browser_type"
        | "browser_scroll"
        | "browser_back"
        | "browser_press"
        | "browser_get_images"
        | "browser_vision"
        | "browser_console" => "browse",
        "text_to_speech" => "generate",
        "execute_code" => "execute",
        "delegate_task" => "delegate",
        "memory" | "session_search" => "retrieve",
        "todo" | "clarify" => "plan",
        _ => "other",
    }
}

fn make_tool_call_id() -> String {
    format!("tc-{}", uuid::Uuid::new_v4().simple())[..15].to_string()
}

fn build_tool_title(tool_name: &str, args: &serde_json::Value) -> String {
    // Simple title: tool_name + first arg value
    match args.get("file_path").and_then(|v| v.as_str()) {
        Some(path) => {
            // Just the filename
            std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string()
        }
        None => match args.get("command").and_then(|v| v.as_str()) {
            Some(cmd) => {
                let truncated: String = cmd.chars().take(40).collect();
                truncated
            }
            None => tool_name.to_string(),
        },
    }
}

// ---- Slash command handlers (mirrors _handle_slash_command) ------------------

fn handle_slash_command(cmd: &str, _session: &SessionState) -> Option<String> {
    let parts: Vec<&str> = cmd.trim().splitn(2, ' ').collect();
    match parts[0] {
        "/help" => Some(format!(
            "Available commands:\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            "/help     — Show this help message",
            "/model    — Show or change current model",
            "/tools    — List available tools",
            "/context  — Show conversation context info",
            "/reset    — Clear conversation history",
            "/compact  — Compress conversation context",
            "/version  — Show Hermes version",
        )),
        "/model" => Some("Model: default (use provider config)".to_string()),
        "/tools" => Some(list_tools_text(),
        ),
        "/context" => Some("Context: no active conversation".to_string()),
        "/reset" => Some("Session reset.".to_string()),
        "/compact" => Some("Context compacted.".to_string()),
        "/version" => Some(format!("hermes-agent {}", env!("CARGO_PKG_VERSION"))),
        _ => None, // Not a slash command or unknown
    }
}

fn list_tools_text() -> String {
    // Return a static list of core tools for ACP display
    let tools = [
        ("read_file", "Read file contents"),
        ("write_file", "Write file contents"),
        ("patch", "Apply a patch to a file"),
        ("search_files", "Search files by pattern"),
        ("terminal", "Run a shell command"),
        ("process", "Manage running processes"),
        ("web_search", "Search the web"),
        ("web_extract", "Extract content from a URL"),
        ("vision_analyze", "Analyze an image"),
        ("image_generate", "Generate an image"),
        ("browser_navigate", "Navigate browser to URL"),
        ("browser_snapshot", "Get page snapshot"),
        ("text_to_speech", "Convert text to speech"),
        ("execute_code", "Execute code in sandbox"),
        ("delegate_task", "Delegate to sub-agent"),
        ("memory", "Read/write persistent memory"),
        ("session_search", "Search past sessions"),
        ("todo", "Manage task list"),
        ("clarify", "Ask clarifying question"),
        ("mixture_of_agents", "Multi-LLM reasoning"),
    ];
    let mut lines = Vec::new();
    for (name, desc) in &tools {
        lines.push(format!("  {name:20} {desc}"));
    }
    format!("Available tools:\n{}", lines.join("\n"))
}

// ---- ACP server dispatch ----------------------------------------------------

impl AcpServer {
    /// Create a new ACP server.
    pub fn new(
        session_manager: Arc<SessionManager>,
        update_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) -> Self {
        Self {
            session_manager,
            update_tx,
            request_id: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Dispatch a JSON-RPC method call to the appropriate handler.
    pub async fn dispatch(
        &self,
        method: &str,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        match method {
            "initialize" => self.initialize(params),
            "authenticate" => self.authenticate(params),
            "newSession" => self.new_session(params),
            "loadSession" => self.load_session(params),
            "resumeSession" => self.resume_session(params),
            "cancel" => self.cancel(params),
            "forkSession" => self.fork_session(params),
            "listSessions" => self.list_sessions(params),
            "prompt" => self.prompt(params).await,
            "setSessionModel" => self.set_session_model(params),
            "setSessionMode" => self.set_session_mode(params),
            "setConfigOption" => self.set_config_option(params),
            other => Err(format!("Unknown method: {other}")),
        }
    }

    // ---- Lifecycle ----------------------------------------------------------

    fn initialize(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let client_info = params
            .get("clientInfo")
            .and_then(|v| v.as_object())
            .map(|obj| {
                format!(
                    "{}{}",
                    obj.get("name").and_then(|v| v.as_str()).unwrap_or("unknown"),
                    obj.get("version")
                        .and_then(|v| v.as_str())
                        .map(|v| format!(" v{v}"))
                        .unwrap_or_default()
                )
            })
            .unwrap_or_else(|| "unknown".to_string());

        tracing::info!("Initialize from {client_info}");

        let resp = InitializeResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_info: Implementation {
                name: AGENT_NAME.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            agent_capabilities: Some(AgentCapabilities {
                load_session: Some(true),
                session_capabilities: Some(SessionCapabilities {
                    fork: Some(SessionForkCapabilities {}),
                    list: Some(SessionListCapabilities {}),
                    resume: Some(SessionResumeCapabilities {}),
                }),
            }),
            auth_methods: None,
        };

        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    fn authenticate(
        &self,
        _params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let resp = AuthenticateResponse {};
        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    // ---- Session management -------------------------------------------------

    fn new_session(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let state = self.session_manager.create_session(&cwd);
        tracing::info!("New session {} (cwd={})", state.session_id, cwd);
        self.send_available_commands_update(&state.session_id);

        let resp = NewSessionResponse {
            session_id: state.session_id.clone(),
        };
        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    fn load_session(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("missing sessionId")?;
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        if self.session_manager.update_cwd(session_id, &cwd).is_none() {
            tracing::warn!("loadSession: session {session_id} not found");
            return Ok(serde_json::Value::Null);
        }

        tracing::info!("Loaded session {session_id}");
        self.send_available_commands_update(session_id);
        serde_json::to_value(LoadSessionResponse {}).map_err(|e| e.to_string())
    }

    fn resume_session(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("missing sessionId")?;
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let state = match self.session_manager.update_cwd(session_id, &cwd) {
            Some(s) => s,
            None => {
                tracing::info!(
                    "resumeSession: session {session_id} not found, creating new"
                );
                self.session_manager.create_session(&cwd)
            }
        };

        tracing::info!("Resumed session {}", state.session_id);
        self.send_available_commands_update(&state.session_id);
        serde_json::to_value(ResumeSessionResponse {}).map_err(|e| e.to_string())
    }

    fn cancel(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("missing sessionId")?;
        if self.session_manager.cancel_session(session_id) {
            tracing::info!("Cancelled session {session_id}");
        }
        Ok(serde_json::Value::Null)
    }

    fn fork_session(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("missing sessionId")?;
        let cwd = params
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();

        let state = self
            .session_manager
            .fork_session(session_id, &cwd)
            .ok_or_else(|| format!("session {session_id} not found"))?;

        tracing::info!("Forked session {} -> {}", session_id, state.session_id);
        self.send_available_commands_update(&state.session_id);

        let resp = ForkSessionResponse {
            session_id: state.session_id.clone(),
        };
        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    fn list_sessions(
        &self,
        _params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let sessions = self
            .session_manager
            .list_sessions()
            .into_iter()
            .map(|(id, cwd)| SessionInfo {
                session_id: id,
                cwd: Some(cwd),
            })
            .collect();

        let resp = ListSessionsResponse { sessions };
        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    // ---- Prompt (core) ------------------------------------------------------

    async fn prompt(
        &self,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        let session_id = params
            .get("sessionId")
            .and_then(|v| v.as_str())
            .ok_or("missing sessionId")?
            .to_string();

        let state = self
            .session_manager
            .get_session(&session_id)
            .ok_or_else(|| format!("session {session_id} not found"))?;

        // Extract user text from content blocks
        let user_text = extract_text_from_prompt(params.get("prompt")).trim().to_string();
        if user_text.is_empty() {
            let resp = PromptResponse {
                stop_reason: Some("end_turn".to_string()),
                usage: None,
            };
            return serde_json::to_value(resp).map_err(|e| e.to_string());
        }

        // Intercept slash commands
        if user_text.starts_with('/') {
            if let Some(response_text) = handle_slash_command(&user_text, &state) {
                self.send_agent_message(&session_id, &response_text);
                let resp = PromptResponse {
                    stop_reason: Some("end_turn".to_string()),
                    usage: None,
                };
                return serde_json::to_value(resp).map_err(|e| e.to_string());
            }
        }

        self.session_manager.clear_cancelled(&session_id);

        tracing::info!(
            "Prompt on session {session_id}: {}",
            &user_text[..user_text.len().min(100)]
        );

        let result = run_agent(&self.session_manager, &self.update_tx, &session_id, &user_text).await;

        let resp = PromptResponse {
            stop_reason: Some(result.stop_reason),
            usage: if result.total_tokens > 0 {
                Some(Usage {
                    input_tokens: Some(result.input_tokens),
                    output_tokens: Some(result.output_tokens),
                    total_tokens: Some(result.total_tokens),
                    thought_tokens: result.thought_tokens,
                    cached_read_tokens: result.cached_read_tokens,
                })
            } else {
                None
            },
        };
        serde_json::to_value(resp).map_err(|e| e.to_string())
    }

    // ---- Config updates -----------------------------------------------------

    fn set_session_model(
        &self,
        _params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(SetSessionModelResponse {})
            .map_err(|e| e.to_string())
    }

    fn set_session_mode(
        &self,
        _params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(SetSessionModeResponse {})
            .map_err(|e| e.to_string())
    }

    fn set_config_option(
        &self,
        _params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        serde_json::to_value(SetConfigOptionResponse {})
            .map_err(|e| e.to_string())
    }

    // ---- Session updates (sent via channel) ---------------------------------

    fn send_available_commands_update(&self, _session_id: &str) {
        let commands = vec![
            AvailableCommand {
                name: "help".to_string(),
                description: "List available commands".to_string(),
                input: None,
            },
            AvailableCommand {
                name: "model".to_string(),
                description: "Show current model and provider, or switch models".to_string(),
                input: Some(UnstructuredCommandInput {
                    hint: Some("model name to switch to".to_string()),
                }),
            },
            AvailableCommand {
                name: "tools".to_string(),
                description: "List available tools with descriptions".to_string(),
                input: None,
            },
            AvailableCommand {
                name: "context".to_string(),
                description: "Show conversation message counts by role".to_string(),
                input: None,
            },
            AvailableCommand {
                name: "reset".to_string(),
                description: "Clear conversation history".to_string(),
                input: None,
            },
            AvailableCommand {
                name: "compact".to_string(),
                description: "Compress conversation context".to_string(),
                input: None,
            },
            AvailableCommand {
                name: "version".to_string(),
                description: "Show Hermes version".to_string(),
                input: None,
            },
        ];

        let update = AvailableCommandsUpdate {
            session_update: "available_commands_update".to_string(),
            available_commands: commands,
        };
        let _ = self.update_tx.send(serde_json::to_value(update).unwrap_or_default());
    }

    fn send_agent_message(&self, session_id: &str, text: &str) {
        let update = serde_json::json!({
            "sessionUpdate": "agent_message",
            "message": [{
                "type": "text",
                "text": text,
            }],
            "sessionId": session_id,
        });
        let _ = self.update_tx.send(update);
    }

    fn send_tool_start(&self, session_id: &str, tool_call_id: &str, tool_name: &str, args: &serde_json::Value) {
        let title = build_tool_title(tool_name, args);
        let kind = get_tool_kind(tool_name);
        let update = serde_json::json!({
            "sessionUpdate": "tool_call_start",
            "toolCallId": tool_call_id,
            "title": title,
            "kind": kind,
            "rawInput": args,
            "sessionId": session_id,
        });
        let _ = self.update_tx.send(update);
    }

    fn send_tool_content(&self, session_id: &str, tool_call_id: &str, text: &str) {
        let update = serde_json::json!({
            "sessionUpdate": "tool_call_content",
            "toolCallId": tool_call_id,
            "content": [{
                "type": "text",
                "text": text,
            }],
            "sessionId": session_id,
        });
        let _ = self.update_tx.send(update);
    }

    fn send_agent_thought(&self, session_id: &str, text: &str) {
        let update = serde_json::json!({
            "sessionUpdate": "agent_thought",
            "thought": text,
            "sessionId": session_id,
        });
        let _ = self.update_tx.send(update);
    }
}

// ---- Agent runner -----------------------------------------------------------

/// Result from running the agent.
struct AgentRunResult {
    stop_reason: String,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    thought_tokens: Option<u64>,
    cached_read_tokens: Option<u64>,
}

/// Resolve the default model from config, env vars, or a hardcoded fallback.
fn resolve_default_model() -> String {
    // 1. Try Hermes config.yaml
    if let Ok(config) = HermesConfig::load() {
        if let Some(model) = config.model.name {
            if !model.is_empty() {
                return model;
            }
        }
    }
    // 2. Try environment variables
    if let Ok(model) = std::env::var("HERMES_DEFAULT_MODEL") {
        if !model.is_empty() {
            return model;
        }
    }
    if let Ok(model) = std::env::var("ANTHROPIC_MODEL") {
        if !model.is_empty() {
            return model;
        }
    }
    if let Ok(model) = std::env::var("OPENAI_MODEL") {
        if !model.is_empty() {
            return model;
        }
    }
    // 3. Hardcoded fallback
    "anthropic/claude-sonnet-4-6".to_string()
}

async fn run_agent(
    session_manager: &SessionManager,
    update_tx: &tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    session_id: &str,
    user_message: &str,
) -> AgentRunResult {
    let default_model = resolve_default_model();

    // Load full config to populate provider/base_url/api_key if available
    let (provider, base_url, api_key) = if let Ok(config) = HermesConfig::load() {
        (
            config.model.provider,
            config.model.base_url,
            config.model.api_key,
        )
    } else {
        (None, None, None)
    };

    let config = AgentConfig {
        model: default_model,
        provider,
        base_url,
        api_key,
        api_mode: None,
        max_iterations: 20,
        skip_context_files: false,
        platform: Some("hermes-acp".to_string()),
        session_id: Some(session_id.to_string()),
        enable_caching: false,
        compression_enabled: false,
        compression_config: None,
        terminal_cwd: None,
        ephemeral_system_prompt: None,
        memory_nudge_interval: 0,  // disabled in ACP
        skill_nudge_interval: 0,   // disabled in ACP
        memory_flush_min_turns: 6,
        self_evolution_enabled: false,  // disabled in ACP
        credential_pool: None,
        fallback_providers: Vec::new(),
        provider_preferences: None,
        session_db: None,
        persist_session: true,
    };

    let mut registry = ToolRegistry::new();
    hermes_tools::register_all_tools(&mut registry);
    let registry_arc = Arc::new(registry);

    let mut agent = match AIAgent::new(config, registry_arc) {
        Ok(a) => a,
        Err(e) => {
            tracing::error!("Failed to create agent for session {session_id}: {e}");
            let error_update = serde_json::json!({
                "sessionUpdate": "agent_message",
                "message": [{
                    "type": "text",
                    "text": format!("Agent initialization error: {e}"),
                }],
                "sessionId": session_id,
            });
            let _ = update_tx.send(error_update);
            return AgentRunResult {
                stop_reason: "end_turn".to_string(),
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                thought_tokens: None,
                cached_read_tokens: None,
            };
        }
    };

    let result = agent.run_conversation(user_message, None, None).await;

    // Send final response
    if !result.response.is_empty() {
        let update = serde_json::json!({
            "sessionUpdate": "agent_message",
            "message": [{
                "type": "text",
                "text": result.response,
            }],
            "sessionId": session_id,
        });
        let _ = update_tx.send(update);
    }

    let stop_reason = if session_manager.is_cancelled(session_id) {
        "cancelled".to_string()
    } else {
        result.exit_reason.clone()
    };

    AgentRunResult {
        stop_reason,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        thought_tokens: None,
        cached_read_tokens: None,
    }
}

// ---- Helpers ----------------------------------------------------------------

fn extract_text_from_prompt(prompt_value: Option<&serde_json::Value>) -> String {
    match prompt_value {
        Some(serde_json::Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for block in blocks {
                if let Some(obj) = block.as_object() {
                    let block_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                parts.push(text.to_string());
                            }
                        }
                        _ => {
                            // Try .text attribute on non-text blocks
                            if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                                parts.push(text.to_string());
                            }
                        }
                    }
                }
            }
            parts.join("\n")
        }
        Some(serde_json::Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;

    fn test_session() -> Arc<SessionState> {
        let mgr = SessionManager::new();
        mgr.create_session(".")
    }

    // ---- get_tool_kind ----

    #[test]
    fn test_get_tool_kind_read() {
        assert_eq!(get_tool_kind("read_file"), "read");
        assert_eq!(get_tool_kind("search_files"), "read");
    }

    #[test]
    fn test_get_tool_kind_edit() {
        assert_eq!(get_tool_kind("write_file"), "edit");
        assert_eq!(get_tool_kind("patch"), "edit");
    }

    #[test]
    fn test_get_tool_kind_execute() {
        assert_eq!(get_tool_kind("terminal"), "execute");
        assert_eq!(get_tool_kind("execute_code"), "execute");
    }

    #[test]
    fn test_get_tool_kind_web() {
        assert_eq!(get_tool_kind("web_search"), "web");
        assert_eq!(get_tool_kind("web_extract"), "web");
    }

    #[test]
    fn test_get_tool_kind_browse() {
        assert_eq!(get_tool_kind("browser_navigate"), "browse");
        assert_eq!(get_tool_kind("browser_snapshot"), "browse");
        assert_eq!(get_tool_kind("browser_click"), "browse");
    }

    #[test]
    fn test_get_tool_kind_vision() {
        assert_eq!(get_tool_kind("vision_analyze"), "vision");
    }

    #[test]
    fn test_get_tool_kind_generate() {
        assert_eq!(get_tool_kind("image_generate"), "generate");
        assert_eq!(get_tool_kind("text_to_speech"), "generate");
    }

    #[test]
    fn test_get_tool_kind_delegate() {
        assert_eq!(get_tool_kind("delegate_task"), "delegate");
    }

    #[test]
    fn test_get_tool_kind_retrieve() {
        assert_eq!(get_tool_kind("memory"), "retrieve");
        assert_eq!(get_tool_kind("session_search"), "retrieve");
    }

    #[test]
    fn test_get_tool_kind_plan() {
        assert_eq!(get_tool_kind("todo"), "plan");
        assert_eq!(get_tool_kind("clarify"), "plan");
    }

    #[test]
    fn test_get_tool_kind_other() {
        assert_eq!(get_tool_kind("unknown_tool"), "other");
    }

    // ---- build_tool_title ----

    #[test]
    fn test_build_tool_title_from_file_path() {
        let args = serde_json::json!({"file_path": "/home/user/project/src/main.rs"});
        assert_eq!(build_tool_title("read_file", &args), "main.rs");
    }

    #[test]
    fn test_build_tool_title_from_command() {
        let args = serde_json::json!({"command": "git status && git log"});
        assert_eq!(build_tool_title("terminal", &args), "git status && git log");
    }

    #[test]
    fn test_build_tool_title_long_command_truncated() {
        let long_cmd = "a".repeat(100);
        let args = serde_json::json!({"command": long_cmd});
        let title = build_tool_title("terminal", &args);
        assert_eq!(title.len(), 40);
    }

    #[test]
    fn test_build_tool_title_fallback() {
        let args = serde_json::json!({"other": "value"});
        assert_eq!(build_tool_title("my_tool", &args), "my_tool");
    }

    // ---- handle_slash_command ----

    #[test]
    fn test_slash_help() {
        let result = handle_slash_command("/help", &test_session());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("/help"));
        assert!(text.contains("/model"));
    }

    #[test]
    fn test_slash_model() {
        let result = handle_slash_command("/model", &test_session());
        assert!(result.is_some());
        assert!(result.unwrap().contains("Model"));
    }

    #[test]
    fn test_slash_tools() {
        let result = handle_slash_command("/tools", &test_session());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("read_file"));
        assert!(text.contains("write_file"));
    }

    #[test]
    fn test_slash_version() {
        let result = handle_slash_command("/version", &test_session());
        assert!(result.is_some());
        assert!(result.unwrap().contains("hermes-agent"));
    }

    #[test]
    fn test_slash_reset() {
        let result = handle_slash_command("/reset", &test_session());
        assert!(result.is_some());
        assert!(result.unwrap().contains("reset"));
    }

    #[test]
    fn test_slash_compact() {
        let result = handle_slash_command("/compact", &test_session());
        assert!(result.is_some());
        assert!(result.unwrap().contains("compacted"));
    }

    #[test]
    fn test_slash_unknown() {
        let result = handle_slash_command("/notfound", &test_session());
        assert!(result.is_none());
    }

    #[test]
    fn test_slash_not_a_command() {
        let result = handle_slash_command("just text", &test_session());
        assert!(result.is_none());
    }

    // ---- extract_text_from_prompt ----

    #[test]
    fn test_extract_text_string() {
        let prompt = serde_json::json!("hello world");
        assert_eq!(extract_text_from_prompt(Some(&prompt)), "hello world");
    }

    #[test]
    fn test_extract_text_array_single_block() {
        let prompt = serde_json::json!([{"type": "text", "text": "hello"}]);
        assert_eq!(extract_text_from_prompt(Some(&prompt)), "hello");
    }

    #[test]
    fn test_extract_text_array_multiple_blocks() {
        let prompt = serde_json::json!([
            {"type": "text", "text": "line1"},
            {"type": "text", "text": "line2"},
        ]);
        assert_eq!(extract_text_from_prompt(Some(&prompt)), "line1\nline2");
    }

    #[test]
    fn test_extract_text_null_prompt() {
        assert_eq!(extract_text_from_prompt(None), "");
    }

    #[test]
    fn test_extract_text_number_prompt() {
        let prompt = serde_json::json!(42);
        assert_eq!(extract_text_from_prompt(Some(&prompt)), "");
    }

    // ---- make_tool_call_id ----

    #[test]
    fn test_tool_call_id_format() {
        let id = make_tool_call_id();
        assert!(id.starts_with("tc-"));
        assert_eq!(id.len(), 15);
    }

    #[test]
    fn test_tool_call_id_unique() {
        let id1 = make_tool_call_id();
        let id2 = make_tool_call_id();
        assert_ne!(id1, id2);
    }
}

