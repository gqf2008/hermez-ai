//! Integration tests for hermez-tools registry and dispatch.

use serde_json::json;

#[test]
fn test_tool_register_and_dispatch() {
    let mut registry = hermez_tools::registry::ToolRegistry::new();
    hermez_tools::register_all_tools(&mut registry);

    assert!(registry.len() > 0);

    // Verify core tools are registered
    let core_tools = vec![
        "read_file",
        "write_file",
        "terminal",
        "web_search",
        "browser_navigate",
    ];
    for name in &core_tools {
        assert!(registry.has(name), "Core tool '{}' not registered", name);
    }
}

#[test]
fn test_toolset_resolution() {
    use hermez_tools::toolsets_def::{resolve_toolset, validate_toolset};

    assert!(validate_toolset("all"));
    assert!(validate_toolset("web"));
    assert!(validate_toolset("file"));
    assert!(!validate_toolset("nonexistent"));

    let web_tools = resolve_toolset("web").unwrap();
    assert!(!web_tools.is_empty());
}

#[test]
fn test_registry_deregister() {
    let mut registry = hermez_tools::registry::ToolRegistry::new();
    hermez_tools::register_all_tools(&mut registry);

    let before = registry.len();
    registry.deregister("image_generate");
    let after = registry.len();
    assert_eq!(before - 1, after);
    assert!(!registry.has("image_generate"));
}
