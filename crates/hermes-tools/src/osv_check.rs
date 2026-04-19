#![allow(dead_code)]
//! OSV malware check for MCP extension packages.
//!
//! Before launching an MCP server via npx/uvx, queries the OSV (Open Source
//! Vulnerabilities) API to check if the package has any known malware advisories
//! (MAL-* IDs). Regular CVEs are ignored — only confirmed malware is blocked.
//!
//! Mirrors the Python `tools/osv_check.py`.

use std::time::Duration;

/// OSV API endpoint.
const OSV_ENDPOINT: &str = "https://api.osv.dev/v1/query";
/// Request timeout in seconds.
const OSV_TIMEOUT: Duration = Duration::from_secs(10);

/// Check if an MCP server package has known malware advisories.
///
/// Inspects the command (e.g. "npx", "uvx") and args to infer the package
/// name and ecosystem. Queries the OSV API for MAL-* advisories.
///
/// Returns `Some(error_message)` if malware is found, `None` if clean/unknown.
/// Returns `None` (fail-open) on network errors or unrecognized commands.
pub async fn check_package_for_malware(command: &str, args: &[String]) -> Option<String> {
    let ecosystem = infer_ecosystem(command)?;
    let (package, version) = parse_package_from_args(args, ecosystem)?;

    match query_osv(&package, ecosystem, version.as_deref()).await {
        Ok(malware) => {
            if malware.is_empty() {
                None
            } else {
                let ids: Vec<String> = malware.iter().take(3).map(|m| m["id"].to_string()).collect();
                let summaries: Vec<String> = malware
                    .iter()
                    .take(3)
                    .map(|m| {
                        m.get("summary")
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| m["id"].to_string())
                    })
                    .collect();
                Some(format!(
                    "BLOCKED: Package '{}' ({}) has known malware advisories: {}. Details: {}",
                    package,
                    ecosystem,
                    ids.join(", "),
                    summaries.join("; ")
                ))
            }
        }
        Err(e) => {
            // Fail-open: network errors allow the package to proceed
            tracing::debug!("OSV check failed for {}/{} (allowing): {}", ecosystem, package, e);
            None
        }
    }
}

/// Infer package ecosystem from the command name.
fn infer_ecosystem(command: &str) -> Option<&'static str> {
    let base = command
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(command)
        .to_lowercase();
    match base.as_str() {
        "npx" | "npx.cmd" => Some("npm"),
        "uvx" | "uvx.cmd" | "pipx" => Some("PyPI"),
        _ => None,
    }
}

/// Extract package name and optional version from command args.
///
/// Returns `Some((package_name, version))` or `None` if not parseable.
fn parse_package_from_args(args: &[String], ecosystem: &str) -> Option<(String, Option<String>)> {
    if args.is_empty() {
        return None;
    }

    // Skip flags to find the package token
    let package_token = args.iter().find(|arg| !arg.starts_with('-'))?;

    if ecosystem == "npm" {
        Some(parse_npm_package(package_token))
    } else if ecosystem == "PyPI" {
        Some(parse_pypi_package(package_token))
    } else {
        Some((package_token.clone(), None))
    }
}

/// Parse npm package: @scope/name@version or name@version.
fn parse_npm_package(token: &str) -> (String, Option<String>) {
    if let Some(after_at) = token.strip_prefix('@') {
        // Scoped: @scope/name@version
        // Find '/' in the part after '@', then optional '@'
        if let Some(slash_pos) = after_at.find('/') {
            let after_slash = &after_at[slash_pos + 1..];
            if let Some(at_pos) = after_slash.find('@') {
                let name = token[..1 + slash_pos + 1 + at_pos].to_string();
                let version = token[1 + slash_pos + 1 + at_pos + 1..].to_string();
                let version = if version == "latest" { None } else { Some(version) };
                return (name, version);
            }
            return (token.to_string(), None);
        }
        return (token.to_string(), None);
    }
    // Unscoped: name@version
    if let Some(at_pos) = token.rfind('@') {
        let name = token[..at_pos].to_string();
        let version = token[at_pos + 1..].to_string();
        let version = if version.is_empty() || version == "latest" {
            None
        } else {
            Some(version)
        };
        return (name, version);
    }
    (token.to_string(), None)
}

/// Parse PyPI package: name==version or name[extras]==version.
fn parse_pypi_package(token: &str) -> (String, Option<String>) {
    // Strip extras and version: name[extra1,extra2]==version
    if let Some(eq_pos) = token.find("==") {
        let name_part = &token[..eq_pos];
        let version = token[eq_pos + 2..].to_string();
        // Strip extras from name
        let name = if let Some(bracket) = name_part.find('[') {
            name_part[..bracket].to_string()
        } else {
            name_part.to_string()
        };
        return (name, Some(version));
    }
    // No version — strip extras if present
    if let Some(bracket) = token.find('[') {
        return (token[..bracket].to_string(), None);
    }
    (token.to_string(), None)
}

/// Query the OSV API for MAL-* advisories.
async fn query_osv(
    package: &str,
    ecosystem: &str,
    version: Option<&str>,
) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
    let mut payload = serde_json::json!({
        "package": {
            "name": package,
            "ecosystem": ecosystem
        }
    });
    if let Some(v) = version {
        payload["version"] = serde_json::json!(v);
    }

    let client = reqwest::Client::builder()
        .timeout(OSV_TIMEOUT)
        .build()?;

    let resp = client
        .post(OSV_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("User-Agent", "hermes-agent-osv-check/1.0")
        .json(&payload)
        .send()
        .await?;

    let result: serde_json::Value = resp.json().await?;

    let vulns = result
        .get("vulns")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Only malware advisories — ignore regular CVEs
    let malware: Vec<serde_json::Value> = vulns
        .into_iter()
        .filter(|v| {
            v.get("id")
                .and_then(|id| id.as_str())
                .map(|id| id.starts_with("MAL-"))
                .unwrap_or(false)
        })
        .collect();

    Ok(malware)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_ecosystem_npx() {
        assert_eq!(infer_ecosystem("npx"), Some("npm"));
        assert_eq!(infer_ecosystem("/usr/bin/npx"), Some("npm"));
        assert_eq!(infer_ecosystem("npx.cmd"), Some("npm"));
    }

    #[test]
    fn test_infer_ecosystem_uvx() {
        assert_eq!(infer_ecosystem("uvx"), Some("PyPI"));
        assert_eq!(infer_ecosystem("pipx"), Some("PyPI"));
    }

    #[test]
    fn test_infer_ecosystem_unknown() {
        assert_eq!(infer_ecosystem("python"), None);
    }

    #[test]
    fn test_parse_npm_unscoped() {
        assert_eq!(parse_npm_package("express"), ("express".to_string(), None));
        assert_eq!(
            parse_npm_package("express@4.18.0"),
            ("express".to_string(), Some("4.18.0".to_string()))
        );
        assert_eq!(
            parse_npm_package("express@latest"),
            ("express".to_string(), None)
        );
    }

    #[test]
    fn test_parse_npm_scoped() {
        assert_eq!(
            parse_npm_package("@types/node"),
            ("@types/node".to_string(), None)
        );
        assert_eq!(
            parse_npm_package("@types/node@18.0.0"),
            ("@types/node".to_string(), Some("18.0.0".to_string()))
        );
    }

    #[test]
    fn test_parse_pypi() {
        assert_eq!(parse_pypi_package("requests"), ("requests".to_string(), None));
        assert_eq!(
            parse_pypi_package("requests==2.31.0"),
            ("requests".to_string(), Some("2.31.0".to_string()))
        );
        assert_eq!(
            parse_pypi_package("requests[socks]==2.31.0"),
            ("requests".to_string(), Some("2.31.0".to_string()))
        );
    }

    #[test]
    fn test_parse_args_empty() {
        assert!(parse_package_from_args(&[], "npm").is_none());
    }

    #[test]
    fn test_parse_args_skip_flags() {
        let args = vec!["-y".to_string(), "express".to_string()];
        let result = parse_package_from_args(&args, "npm");
        assert_eq!(result, Some(("express".to_string(), None)));
    }
}
