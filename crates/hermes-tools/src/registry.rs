//! Central registry for all Hermes Agent tools.
//!
//! Architecture change from Python:
//! - Python: `registry.register()` at module import time (circular-import safe)
//! - Rust: `register_all_tools(&mut registry)` at startup (no import-order issues)
//!
//! Mirrors the Python `tools/registry.py` ToolRegistry class.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Serialize;
use serde_json::Value;

use hermes_core::Result;

/// JSON Schema for tool parameters (wrapped in Arc to avoid deep clones).
pub type ToolSchema = Arc<Value>;

/// Tool handler function signature.
///
/// Takes a JSON Value of arguments, returns a JSON string result.
pub type ToolHandler = dyn Fn(Value) -> Result<String> + Send + Sync;

/// Availability check function.
///
/// Returns true if the tool's prerequisites are met (env vars set,
/// dependencies installed, etc.).
pub type CheckFn = dyn Fn() -> bool + Send + Sync;

/// Metadata for a single registered tool.
///
/// Mirrors the Python `ToolEntry` class.
pub struct ToolEntry {
    /// Tool name (must be unique across the registry).
    pub name: String,
    /// Toolset this tool belongs to (e.g., "file", "web", "terminal").
    pub toolset: String,
    /// JSON Schema for the tool's parameters.
    pub schema: ToolSchema,
    /// Handler function that executes the tool.
    pub handler: Arc<ToolHandler>,
    /// Availability check function.
    pub check_fn: Option<Arc<CheckFn>>,
    /// Environment variables this tool requires.
    pub requires_env: Vec<String>,
    /// Whether the tool is async (currently unused in Rust, all handlers are sync).
    pub is_async: bool,
    /// Human-readable description.
    pub description: String,
    /// Emoji icon for display.
    pub emoji: String,
    /// Maximum result size in characters.
    pub max_result_size_chars: Option<usize>,
}

/// Central registry that collects tool schemas and handlers.
///
/// Mirrors the Python `ToolRegistry` class.
#[derive(Clone)]
pub struct ToolRegistry {
    /// All registered tools, keyed by name.
    tools: Arc<RwLock<HashMap<String, Arc<ToolEntry>>>>,
    /// Toolset availability checks, keyed by toolset name.
    toolset_checks: Arc<RwLock<HashMap<String, Arc<CheckFn>>>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Create a new empty registry.
    pub fn new() -> Self {
        Self {
            tools: Arc::new(RwLock::new(HashMap::new())),
            toolset_checks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a tool in the registry.
    ///
    /// If a tool with the same name exists in a different toolset,
    /// a warning is logged and the entry is overwritten.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &self,
        name: String,
        toolset: String,
        schema: Value,
        handler: Arc<ToolHandler>,
        check_fn: Option<Arc<CheckFn>>,
        requires_env: Vec<String>,
        description: String,
        emoji: String,
        max_result_size_chars: Option<usize>,
    ) {
        let mut tools = self.tools.write();
        // Check for name collisions
        if let Some(existing) = tools.get(&name) {
            if existing.toolset != toolset {
                tracing::warn!(
                    "Tool name collision: '{}' (toolset '{}') is being overwritten by toolset '{}'",
                    name, existing.toolset, toolset
                );
            }
        }

        // Store toolset check if provided
        if let Some(check) = &check_fn {
            let mut checks = self.toolset_checks.write();
            if !checks.contains_key(&toolset) {
                checks.insert(toolset.clone(), Arc::clone(check));
            }
        }

        let entry = ToolEntry {
            name: name.clone(),
            toolset,
            schema: Arc::new(schema),
            handler,
            check_fn,
            requires_env,
            is_async: false,
            description,
            emoji,
            max_result_size_chars,
        };

        tools.insert(name, Arc::new(entry));
    }

    /// Remove a tool from the registry.
    ///
    /// Used by MCP dynamic tool discovery when a server sends
    /// `notifications/tools/list_changed`.
    pub fn deregister(&self, name: &str) {
        let mut tools = self.tools.write();
        if let Some(entry) = tools.remove(name) {
            // Clean up toolset check if no other tools remain in the same toolset
            let toolset = &entry.toolset;
            let still_has_tools = tools.values().any(|e| e.toolset == *toolset);
            drop(tools);
            if !still_has_tools {
                self.toolset_checks.write().remove(toolset);
            }
        }
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<ToolEntry>> {
        self.tools.read().get(name).cloned()
    }

    /// Check if a tool exists by name.
    pub fn has(&self, name: &str) -> bool {
        self.tools.read().contains_key(name)
    }

    /// Dispatch a tool call by name.
    ///
    /// Looks up the tool, validates it exists, and calls its handler.
    pub fn dispatch(&self, name: &str, args: Value) -> Result<String> {
        let tool = self
            .tools
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| hermes_core::HermesError::new(
                hermes_core::errors::ErrorCategory::ToolError,
                format!("Unknown tool: {name}"),
            ))?;

        (tool.handler)(args)
    }

    /// Get JSON schema definitions for tools.
    ///
    /// Only includes tools whose `check_fn` passes (i.e., prerequisites are met).
    /// Returns schemas in the OpenAI function-calling format.
    pub fn get_definitions(&self, include_names: Option<&[String]>) -> Vec<ToolSchema> {
        let mut defs = Vec::new();

        for entry in self.tools.read().values() {
            // Skip if not in the inclusion list
            if let Some(names) = include_names {
                if !names.contains(&entry.name) {
                    continue;
                }
            }

            // Skip if availability check fails
            if let Some(check) = &entry.check_fn {
                if !check() {
                    continue;
                }
            }

            // Wrap in OpenAI function-calling format
            // Clone the inner Value for mutation (schema storage is Arc'd, so this
            // is cheaper than before when entry.schema was a bare Value).
            let mut schema = (*entry.schema).clone();
            if let Some(obj) = schema.as_object_mut() {
                obj.insert("name".to_string(), Value::String(entry.name.clone()));
            }

            defs.push(Arc::new(serde_json::json!({
                "type": "function",
                "function": schema,
            })));
        }

        defs
    }

    /// List all registered tool names.
    pub fn list_tools(&self) -> Vec<String> {
        self.tools.read().keys().cloned().collect()
    }

    /// List all registered toolsets.
    pub fn list_toolsets(&self) -> Vec<String> {
        self.toolset_checks.read().keys().cloned().collect()
    }

    /// Get all tools (entries) that pass their availability checks.
    pub fn get_available_tools(&self) -> Vec<Arc<ToolEntry>> {
        self.tools
            .read()
            .values()
            .filter(|entry| {
                // Check availability function
                if let Some(check) = &entry.check_fn {
                    if !check() {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Get a tool handler by name.
    ///
    /// Used by subagent manager to copy handlers into child registries.
    pub fn get_handler(&self, name: &str) -> Option<Arc<ToolHandler>> {
        self.tools.read().get(name).map(|entry| Arc::clone(&entry.handler))
    }

    /// Get the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.read().len()
    }

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.read().is_empty()
    }
}

/// Global singleton registry instance.
///
/// Mirrors the Python module-level `registry = ToolRegistry()` singleton.
/// Uses `parking_lot::Mutex` for thread-safe access during tool registration
/// and dispatch.
pub fn global_registry() -> &'static parking_lot::Mutex<ToolRegistry> {
    use std::sync::OnceLock;
    static REGISTRY: OnceLock<parking_lot::Mutex<ToolRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| parking_lot::Mutex::new(ToolRegistry::new()))
}

/// Helper to create a tool result JSON string.
pub fn tool_result(value: impl Serialize) -> Result<String> {
    serde_json::to_string(&value).map_err(|e| {
        hermes_core::HermesError::with_source(
            hermes_core::errors::ErrorCategory::ToolError,
            "Failed to serialize tool result",
            e.into(),
        )
    })
}

/// Helper to create a tool error JSON string.
pub fn tool_error(message: impl Into<String>) -> String {
    serde_json::json!({ "error": message.into() }).to_string()
}

/// Helper to create a tool result with text output.
pub fn tool_text_result(text: impl Into<String>) -> Result<String> {
    tool_result(serde_json::json!({ "output": text.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_handler(result: &'static str) -> Arc<ToolHandler> {
        Arc::new(move |_args| tool_text_result(result))
    }

    #[test]
    fn test_register_and_dispatch() {
        let mut registry = ToolRegistry::new();
        registry.register(
            "test_tool".to_string(),
            "test".to_string(),
            serde_json::json!({
                "name": "test_tool",
                "description": "A test tool",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }),
            make_handler("hello"),
            None,
            vec![],
            "A test tool".to_string(),
            "🧪".to_string(),
            None,
        );

        assert!(registry.has("test_tool"));
        assert_eq!(registry.len(), 1);

        let result = registry.dispatch("test_tool", serde_json::json!({})).unwrap();
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_deregister() {
        let mut registry = ToolRegistry::new();
        registry.register(
            "tool_a".to_string(),
            "set_a".to_string(),
            serde_json::json!({"name": "tool_a"}),
            make_handler("a"),
            None,
            vec![],
            String::new(),
            String::new(),
            None,
        );

        assert_eq!(registry.len(), 1);
        registry.deregister("tool_a");
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_get_definitions_filters_by_check_fn() {
        let mut registry = ToolRegistry::new();

        // Tool with passing check
        registry.register(
            "available_tool".to_string(),
            "test".to_string(),
            serde_json::json!({"name": "available_tool"}),
            make_handler("ok"),
            Some(Arc::new(|| true)),
            vec![],
            String::new(),
            String::new(),
            None,
        );

        // Tool with failing check
        registry.register(
            "unavailable_tool".to_string(),
            "test".to_string(),
            serde_json::json!({"name": "unavailable_tool"}),
            make_handler("nope"),
            Some(Arc::new(|| false)),
            vec![],
            String::new(),
            String::new(),
            None,
        );

        let defs = registry.get_definitions(None);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["function"]["name"], "available_tool");
    }
}
