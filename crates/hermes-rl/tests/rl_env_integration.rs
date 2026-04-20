//! Integration tests for hermes-rl environments.

use hermes_rl::Environment;

#[test]
fn test_math_env_name() {
    let env = hermes_rl::math_env::MathEnv::new();
    assert!(!env.name().is_empty());
}

#[test]
fn test_tool_use_env_name() {
    let env = hermes_rl::tool_use_env::ToolUseEnv::new();
    assert!(!env.name().is_empty());
}

#[test]
fn test_web_research_env_name() {
    let env = hermes_rl::web_research_env::WebResearchEnv::new();
    assert!(!env.name().is_empty());
}

#[test]
fn test_atropos_env_name() {
    let env = hermes_rl::atropos_env::AtroposEnv::new();
    assert!(!env.name().is_empty());
}
