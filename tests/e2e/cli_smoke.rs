//! E2E smoke tests for the Hermez CLI binary.
//!
//! These tests invoke the actual `hermez` binary and verify basic behavior.

use std::process::Command;

fn hermez_bin() -> Command {
    let mut cmd = Command::new("cargo");
    cmd.args(["run", "--bin", "hermez", "--"]);
    cmd
}

#[test]
#[ignore = "requires cargo build"]
fn test_cli_version_flag() {
    let output = hermez_bin().arg("--version").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hermez") || output.status.success());
}

#[test]
#[ignore = "requires cargo build"]
fn test_cli_help_flag() {
    let output = hermez_bin().arg("--help").output().unwrap();
    assert!(output.status.success());
}
