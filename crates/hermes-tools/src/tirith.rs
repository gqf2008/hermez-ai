#![allow(dead_code)]
//! Tirith security scanner integration.
//!
//! Mirrors the Python `tools/tirith_security.py`.
//! Pre-execution security scanner that checks shell commands for threats
//! (homograph URLs, pipe-to-interpreter, terminal injection).
//!
//! Not a registered tool — called by the terminal/tool approval pipeline.

use std::path::PathBuf;

use hermes_core::hermes_home::get_hermes_home;

/// Security check result.
#[derive(Debug, Clone)]
pub struct SecurityCheckResult {
    pub action: String, // "allow", "warn", "block"
    pub findings: Vec<String>,
    pub summary: String,
}

/// Get the tirith binary path.
fn tirith_binary_path() -> PathBuf {
    get_hermes_home().join("bin").join("tirith")
}

/// Check if tirith is installed.
pub fn is_tirith_installed() -> bool {
    tirith_binary_path().exists()
}

/// Run a security check on a command string.
pub fn check_command_security(command: &str) -> Result<SecurityCheckResult, String> {
    if command.trim().is_empty() {
        return Err("Command cannot be empty.".to_string());
    }

    let binary = tirith_binary_path();
    if !binary.exists() {
        return Err(format!(
            "Tirith not installed. Install with: `hermes tools install tirith` or run tirith from {}.",
            binary.display()
        ));
    }

    let output = std::process::Command::new(&binary)
        .args(["check", "--json", "--"])
        .arg(command)
        .output()
        .map_err(|e| format!("Failed to run tirith: {e}"))?;

    // Exit code: 0 = allow, 1 = block, 2 = warn
    let exit_code = output.status.code().unwrap_or(-1);
    let action = match exit_code {
        0 => "allow",
        1 => "block",
        2 => "warn",
        _ => "allow", // fail-open by default
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    let mut findings = Vec::new();
    let mut summary = String::new();

    // Parse JSON output if available
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
        if let Some(f) = json.get("findings").and_then(|v| v.as_array()) {
            findings = f
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
        if let Some(s) = json.get("summary").and_then(|v| v.as_str()) {
            summary = s.to_string();
        }
    }

    // Fallback: use stderr/stdout as findings if no JSON
    if findings.is_empty() && !stderr.is_empty() {
        for line in stderr.lines() {
            if !line.trim().is_empty() {
                findings.push(line.trim().to_string());
            }
        }
    }

    if summary.is_empty() {
        summary = format!(
            "Tirith verdict: {action} (exit code: {exit_code})"
        );
    }

    Ok(SecurityCheckResult {
        action: action.to_string(),
        findings,
        summary,
    })
}

/// Ensure tirith is installed (synchronous check).
pub fn ensure_tirith_installed() -> Result<(), String> {
    if is_tirith_installed() {
        return Ok(());
    }

    Err(format!(
        "Tirith binary not found at {}. Install it first.",
        tirith_binary_path().display()
    ))
}

/// Check tirith security requirements for tool availability.
pub fn check_tirith_requirements() -> bool {
    is_tirith_installed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tirith_not_installed() {
        // In test environment, tirith binary is almost certainly not present
        let installed = is_tirith_installed();
        if !installed {
            let result = check_command_security("echo hello");
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("not installed"));
        }
    }

    #[test]
    fn test_check_empty_command() {
        let result = check_command_security("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_check_whitespace_command() {
        let result = check_command_security("   ");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("empty"));
    }

    #[test]
    fn test_ensure_not_installed() {
        if !is_tirith_installed() {
            let result = ensure_tirith_installed();
            assert!(result.is_err());
            assert!(result.unwrap_err().contains("not found"));
        }
    }

    #[test]
    fn test_check_tirith_requirements() {
        // May or may not pass depending on environment
        let _ = check_tirith_requirements();
    }

    #[test]
    fn test_binary_path() {
        let path = tirith_binary_path();
        assert!(path.ends_with("tirith"));
        assert!(path.starts_with(get_hermes_home()));
    }
}
