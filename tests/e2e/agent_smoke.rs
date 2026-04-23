//! E2E smoke tests for the Hermez agent binary.

use std::process::Command;

#[test]
#[ignore = "requires cargo build and API keys"]
fn test_agent_binary_runs() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "hermez-agent", "--", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
}
