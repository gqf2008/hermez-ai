//! Constants and static tool sets for AIAgent.
//!
//! Mirrors Python `_NEVER_PARALLEL_TOOLS`, `_PARALLEL_SAFE_TOOLS`,
//! `_PATH_SCOPED_TOOLS` (run_agent.py:216-245).

use std::collections::HashSet;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::Value;

use hermez_tools::registry::ToolRegistry;
use crate::subagent::{SubagentManager, SubagentResult};

/// Tools that must NEVER run in parallel.
/// `clarify` requires interactive user input — cannot be concurrent.
pub(crate) static NEVER_PARALLEL_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    let mut set = HashSet::new();
    set.insert("clarify");
    set
});

/// Read-only tools with no shared mutable session state.
pub(crate) static PARALLEL_SAFE_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "ha_get_state",
        "ha_list_entities",
        "ha_list_services",
        "read_file",
        "search_files",
        "session_search",
        "skill_view",
        "skills_list",
        "vision_analyze",
        "web_extract",
        "web_search",
    ]
    .into_iter()
    .collect()
});

/// File-scoped tools: can run concurrently when targeting independent paths.
pub(crate) static PATH_SCOPED_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    ["read_file", "write_file", "patch"]
        .into_iter()
        .collect()
});

/// Dispatch subagent delegation in a separate tokio task to break
/// the type-level cycle between execute_tool_call and execute_delegation.
pub(crate) fn dispatch_delegation(
    mgr: Arc<SubagentManager>,
    registry: Arc<ToolRegistry>,
    args: Value,
) -> tokio::sync::oneshot::Receiver<Vec<SubagentResult>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let results = mgr.execute_delegation(args, registry).await;
        let _ = tx.send(results);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_never_parallel_tools_contains_clarify() {
        assert!(NEVER_PARALLEL_TOOLS.contains("clarify"));
    }

    #[test]
    fn test_never_parallel_tools_excludes_others() {
        assert!(!NEVER_PARALLEL_TOOLS.contains("read_file"));
        assert!(!NEVER_PARALLEL_TOOLS.contains("terminal"));
    }

    #[test]
    fn test_parallel_safe_tools_contains_expected() {
        assert!(PARALLEL_SAFE_TOOLS.contains("read_file"));
        assert!(PARALLEL_SAFE_TOOLS.contains("web_search"));
        assert!(PARALLEL_SAFE_TOOLS.contains("session_search"));
        assert!(PARALLEL_SAFE_TOOLS.contains("vision_analyze"));
    }

    #[test]
    fn test_parallel_safe_tools_excludes_clarify() {
        assert!(!PARALLEL_SAFE_TOOLS.contains("clarify"));
    }

    #[test]
    fn test_path_scoped_tools_contains_expected() {
        assert!(PATH_SCOPED_TOOLS.contains("read_file"));
        assert!(PATH_SCOPED_TOOLS.contains("write_file"));
        assert!(PATH_SCOPED_TOOLS.contains("patch"));
    }

    #[test]
    fn test_path_scoped_tools_excludes_others() {
        assert!(!PATH_SCOPED_TOOLS.contains("terminal"));
        assert!(!PATH_SCOPED_TOOLS.contains("web_search"));
    }
}
