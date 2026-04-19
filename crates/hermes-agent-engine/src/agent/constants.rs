//! Constants and static tool sets for AIAgent.
//!
//! Mirrors Python `_NEVER_PARALLEL_TOOLS`, `_PARALLEL_SAFE_TOOLS`,
//! `_PATH_SCOPED_TOOLS` (run_agent.py:216-245).

use std::collections::HashSet;
use std::sync::Arc;

use once_cell::sync::Lazy;
use serde_json::Value;

use hermes_tools::registry::ToolRegistry;
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
