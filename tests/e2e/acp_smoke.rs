//! E2E smoke tests for the Hermez ACP binary.

use std::process::Command;

#[test]
#[ignore = "requires cargo build"]
fn test_acp_binary_help() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "hermez-acp", "--", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
}
