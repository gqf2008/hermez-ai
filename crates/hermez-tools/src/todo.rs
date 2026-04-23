#![allow(dead_code)]
//! Todo list management tool.
//!
//! In-memory task list for planning and task management across a session.
//! Mirrors the Python `tools/todo_tool.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::registry::{tool_error, tool_result, ToolRegistry};
use hermez_core::Result;

/// Valid todo statuses.
const VALID_STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

/// A single todo item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

/// Thread-safe todo store.
#[derive(Debug, Default)]
pub struct TodoStore {
    items: parking_lot::Mutex<Vec<TodoItem>>,
}

impl TodoStore {
    pub fn new() -> Self {
        Self {
            items: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// Write or merge todo items.
    ///
    /// If `merge` is false, replaces the entire list.
    /// If `merge` is true, updates existing items by id and appends new ones.
    pub fn write(&self, todos: Vec<TodoItem>, merge: bool) -> Vec<TodoItem> {
        let mut items = self.items.lock();

        if merge {
            // Update existing items by id, append new ones
            for new_item in todos {
                if let Some(existing) = items.iter_mut().find(|i| i.id == new_item.id) {
                    // Update fields if provided (non-empty)
                    if !new_item.content.is_empty() {
                        existing.content = new_item.content;
                    }
                    if !new_item.status.is_empty() {
                        existing.status = new_item.status;
                    }
                } else {
                    items.push(new_item);
                }
            }
        } else {
            // Replace entire list
            *items = todos;
        }

        items.clone()
    }

    /// Read all todo items.
    pub fn read(&self) -> Vec<TodoItem> {
        self.items.lock().clone()
    }

    /// Check if any items exist.
    pub fn has_items(&self) -> bool {
        !self.items.lock().is_empty()
    }

    /// Format for injection into the system prompt after context compression.
    pub fn format_for_injection(&self) -> String {
        let items = self.items.lock();
        if items.is_empty() {
            return String::new();
        }

        let mut parts = Vec::new();
        for item in items.iter() {
            let marker = match item.status.as_str() {
                "pending" => "[ ]",
                "in_progress" => "[>]",
                "completed" => "[x]",
                "cancelled" => "[~]",
                _ => "[?]",
            };
            parts.push(format!("{} {} ({})", marker, item.content, item.id));
        }

        format!("Todo list:\n{}", parts.join("\n"))
    }
}

/// Todo tool JSON schema.
pub fn todo_schema() -> Value {
    serde_json::json!({
        "name": "todo",
        "description": "Manage a todo list. Pass todos to update, or call with no arguments to read the current list.",
        "parameters": {
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string" },
                            "content": { "type": "string" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"]
                            }
                        },
                        "required": ["id", "content", "status"]
                    }
                },
                "merge": {
                    "type": "boolean",
                    "default": false
                }
            }
        }
    })
}

/// Normalize and validate a todo item.
fn normalize_item(item: &mut TodoItem) {
    if item.status.is_empty() || !VALID_STATUSES.contains(&item.status.as_str()) {
        item.status = "pending".to_string();
    }
}

/// Handle the todo tool call.
pub fn handle_todo(store: &TodoStore, args: Value) -> Result<String> {
    // Read-only mode if no todos provided
    let Some(todos_arr) = args.get("todos").and_then(|v| v.as_array()) else {
        let items = store.read();
        let summary = build_summary(&items);
        return tool_result(serde_json::json!({
            "todos": items,
            "summary": summary,
        }));
    };

    let merge = args.get("merge").and_then(|v| v.as_bool()).unwrap_or(false);

    // Parse and normalize todo items
    let mut todos: Vec<TodoItem> = Vec::new();
    for item_val in todos_arr {
        let mut item: TodoItem = match serde_json::from_value(item_val.clone()) {
            Ok(i) => i,
            Err(e) => return Ok(tool_error(format!("Invalid todo item: {e}"))),
        };
        normalize_item(&mut item);
        todos.push(item);
    }

    let updated = store.write(todos, merge);
    let summary = build_summary(&updated);

    tool_result(serde_json::json!({
        "todos": updated,
        "summary": summary,
    }))
}

/// Build a summary of todo counts by status.
fn build_summary(items: &[TodoItem]) -> serde_json::Value {
    let mut counts: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
    counts.insert("total".to_string(), serde_json::json!(items.len()));

    for status in VALID_STATUSES {
        let count = items.iter().filter(|i| i.status == *status).count();
        counts.insert((*status).to_string(), serde_json::json!(count));
    }

    serde_json::Value::Object(counts)
}

/// Global todo store instance.
pub fn global_todo_store() -> &'static TodoStore {
    use std::sync::OnceLock;
    static STORE: OnceLock<TodoStore> = OnceLock::new();
    STORE.get_or_init(TodoStore::new)
}

/// Register the todo tool.
pub fn register(registry: &mut ToolRegistry) {
    let store = global_todo_store();
    let handler = std::sync::Arc::new(move |args| handle_todo(store, args));

    registry.register(
        "todo".to_string(),
        "organization".to_string(),
        todo_schema(),
        handler,
        None,
        vec![],
        "Manage a todo list for task planning".to_string(),
        "✅".to_string(),
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_replace_todos() {
        let store = TodoStore::new();
        let result = handle_todo(&store, serde_json::json!({
            "todos": [
                { "id": "1", "content": "task a", "status": "pending" },
                { "id": "2", "content": "task b", "status": "in_progress" }
            ]
        })).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["summary"]["total"], 2);
        assert_eq!(parsed["summary"]["pending"], 1);
    }

    #[test]
    fn test_merge_todos() {
        let store = TodoStore::new();

        // First write
        handle_todo(&store, serde_json::json!({
            "todos": [
                { "id": "1", "content": "task a", "status": "pending" }
            ]
        })).unwrap();

        // Merge: update status + add new
        let result = handle_todo(&store, serde_json::json!({
            "merge": true,
            "todos": [
                { "id": "1", "content": "task a", "status": "completed" },
                { "id": "3", "content": "task c", "status": "pending" }
            ]
        })).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["summary"]["total"], 2);
        assert_eq!(parsed["summary"]["completed"], 1);
        assert_eq!(parsed["summary"]["pending"], 1);
    }

    #[test]
    fn test_read_only() {
        let store = TodoStore::new();
        // First populate
        handle_todo(&store, serde_json::json!({
            "todos": [
                { "id": "1", "content": "task a", "status": "pending" }
            ]
        })).unwrap();

        // Read-only
        let result = handle_todo(&store, serde_json::json!({})).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["todos"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_format_for_injection() {
        let store = TodoStore::new();
        store.write(vec![
            TodoItem { id: "1".to_string(), content: "do thing".to_string(), status: "pending".to_string() },
            TodoItem { id: "2".to_string(), content: "done thing".to_string(), status: "completed".to_string() },
        ], false);

        let injection = store.format_for_injection();
        assert!(injection.contains("[ ] do thing"));
        assert!(injection.contains("[x] done thing"));
    }

    #[test]
    fn test_normalize_invalid_status() {
        let store = TodoStore::new();
        let result = handle_todo(&store, serde_json::json!({
            "todos": [
                { "id": "1", "content": "task", "status": "invalid_status" }
            ]
        })).unwrap();
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["todos"][0]["status"], "pending");
    }
}
