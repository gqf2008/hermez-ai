//! Welcome banner, ASCII art, and update check for the CLI.
//!
//! Pure display functions with no HermezCLI state dependency.
//! Mirrors the Python `hermez_cli/banner.py`.

use console::Style;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

// ─── ASCII Art ────────────────────────────────────────────────────────────

/// Hermez Agent logo in ASCII block art.
/// Styled with gold/bronze gradients.
pub const HERMEZ_AGENT_LOGO: &str = r#"
    ██╗  ██╗███████╗██████╗ ███╗   ███╗███████╗███████╗       █████╗  ██████╗ ███████╗███╗   ██╗████████╗
    ██║  ██║██╔════╝██╔══██╗████╗ ████║██╔════╝██╔════╝      ██╔══██╗██╔════╝ ██╔════╝████╗  ██║╚══██╔══╝
    ███████║█████╗  ██████╔╝██╔████╔██║█████╗  ███████╗█████╗███████║██║  ███╗█████╗  ██╔██╗ ██║   ██║
    ██╔══██║██╔══╝  ██╔══██╗██║╚██╔╝██║██╔══╝  ╚════██║╚════╝██╔══██║██║   ██║██╔══╝  ██║╚██╗██║   ██║
    ██║  ██║███████╗██║  ██║██║ ╚═╝ ██║███████╗███████║      ██║  ██║╚██████╔╝███████╗██║ ╚████║   ██║
    ╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝╚═╝     ╚═╝╚══════╝╚══════╝      ╚═╝  ╚═╝ ╚═════╝ ╚══════╝╚═╝  ╚═══╝   ╚═╝
"#;

/// Caduceus symbol in Braille patterns.
pub const HERMEZ_CADUCEUS: &str = r#"
              ⢀⣀⡀⠀⣀⣀⠀⢀⣀⡀
    ⢀⣠⣴⣾⣿⣿⣇⠸⣿⣿⠇⣸⣿⣿⣷⣦⣄⡀
  ⢀⣠⣴⣶⠿⠋⣩⡿⣿⡿⠻⣿⡇⢠⡄⢸⣿⠟⢿⣿⢿⣍⠙⠿⣶⣦⣄⡀
    ⠉⠉⠁⠶⠟⠋⠀⠉⠀⢀⣈⣁⡈⢁⣈⣁⡀⠀⠉⠀⠙⠻⠶⠈⠉⠉
            ⣴⣿⡿⠛⢁⡈⠛⢿⣿⣦
            ⠿⣿⣦⣤⣈⠁⢠⣴⣿⠿
              ⠈⠉⠻⢿⣿⣦⡉⠁
                ⠘⢷⣦⣈⠛⠃
              ⢠⣴⠦⠈⠙⠿⣦⡄
              ⠸⣿⣤⡈⠁⢤⣿⠇
"#;

// ─── Skin-aware color helpers ─────────────────────────────────────────────

#[allow(dead_code)]
fn skin_color(key: &str, fallback: &str) -> String {
    let skin = crate::skin_engine::get_active_skin();
    skin.get_color(key, fallback)
}

#[allow(dead_code)]
fn skin_branding(key: &str, fallback: &str) -> String {
    let skin = crate::skin_engine::get_active_skin();
    skin.get_branding(key, fallback)
}

// ─── Update check ─────────────────────────────────────────────────────────

const UPDATE_CHECK_CACHE_SECONDS: u64 = 6 * 3600;

/// Check how many commits behind origin/main the local repo is.
///
/// Does a `git fetch` at most once every 6 hours (cached to
/// `~/.hermez/.update_check`). Returns the number of commits behind,
/// or `None` if the check fails or isn't applicable.
pub fn check_for_updates() -> Option<u32> {
    let hermez_home = hermez_core::get_hermez_home();
    let repo_dir = hermez_home.join("hermez-agent");
    let cache_file = hermez_home.join(".update_check");

    // Must be a git repo — fall back to project root for dev installs
    let repo_dir = if repo_dir.join(".git").exists() {
        repo_dir
    } else {
        // Try to find the git repo from current executable or working dir
        std::env::current_dir().ok()?.join("hermez-agent")
    };
    if !repo_dir.join(".git").exists() {
        return None;
    }

    // Read cache
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();

    if cache_file.exists() {
        if let Ok(content) = std::fs::read_to_string(&cache_file) {
            if let Ok(cached) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(ts) = cached.get("ts").and_then(|v| v.as_u64()) {
                    if now - ts < UPDATE_CHECK_CACHE_SECONDS {
                        return cached.get("behind").and_then(|v| v.as_u64()).map(|v| v as u32);
                    }
                }
            }
        }
    }

    // Fetch latest refs (fast — only downloads ref metadata, no files)
    let _ = Command::new("git")
        .args(["fetch", "origin", "--quiet"])
        .current_dir(&repo_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Count commits behind
    let behind = Command::new("git")
        .args(["rev-list", "--count", "HEAD..origin/main"])
        .current_dir(&repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8_lossy(&out.stdout).trim().parse::<u32>().ok()
            } else {
                None
            }
        });

    // Write cache
    let cache = serde_json::json!({"ts": now, "behind": behind});
    let _ = std::fs::write(&cache_file, cache.to_string());

    behind
}

/// Resolve the active Hermez git checkout, or None if this isn't a git install.
fn resolve_repo_dir() -> Option<PathBuf> {
    let hermez_home = hermez_core::get_hermez_home();
    let repo_dir = hermez_home.join("hermez-agent");
    if repo_dir.join(".git").exists() {
        return Some(repo_dir);
    }
    // Try current dir
    let cwd = std::env::current_dir().ok()?;
    if cwd.join(".git").exists() {
        return Some(cwd);
    }
    None
}

fn git_short_hash(repo_dir: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=8", rev])
        .current_dir(repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if value.is_empty() {
        return None;
    }
    Some(value)
}

/// Return upstream/local git hashes for the startup banner.
pub fn get_git_banner_state() -> Option<GitBannerState> {
    let repo_dir = resolve_repo_dir()?;

    let upstream = git_short_hash(&repo_dir, "origin/main")?;
    let local = git_short_hash(&repo_dir, "HEAD")?;

    let ahead = Command::new("git")
        .args(["rev-list", "--count", "origin/main..HEAD"])
        .current_dir(&repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<u32>()
                    .ok()
            } else {
                Some(0)
            }
        })
        .unwrap_or(0);

    Some(GitBannerState { upstream, local, ahead })
}

#[derive(Debug, Clone)]
pub struct GitBannerState {
    pub upstream: String,
    pub local: String,
    pub ahead: u32,
}

/// Return the version label shown in the startup banner title.
pub fn format_banner_version_label() -> String {
    let version = env!("CARGO_PKG_VERSION");
    let base = format!("Hermez Agent v{version}");

    let state = get_git_banner_state();
    if state.is_none() {
        return base;
    }

    let state = state.unwrap();
    let upstream = state.upstream;
    let local = state.local;
    let ahead = state.ahead;

    if ahead == 0 || upstream == local {
        return format!("{base} · upstream {upstream}");
    }

    let carried_word = if ahead == 1 { "commit" } else { "commits" };
    format!("{base} · upstream {upstream} · local {local} (+{ahead} carried {carried_word})")
}

// ─── Welcome banner ───────────────────────────────────────────────────────

/// Format a token count for display (e.g. 128000 → "128K", 1048576 → "1M").
pub fn format_context_length(tokens: usize) -> String {
    if tokens >= 1_000_000 {
        let val = tokens as f64 / 1_000_000.0;
        let rounded = val.round();
        if (val - rounded).abs() < 0.05 {
            return format!("{:.0}M", rounded);
        }
        return format!("{val:.1}M");
    } else if tokens >= 1_000 {
        let val = tokens as f64 / 1_000.0;
        let rounded = val.round();
        if (val - rounded).abs() < 0.05 {
            return format!("{:.0}K", rounded);
        }
        return format!("{val:.1}K");
    }
    tokens.to_string()
}

/// Display the welcome banner.
///
/// Shows the Hermez logo, model info, available tools/skills summary,
/// and optional update notification.
pub fn print_welcome_banner(
    model: &str,
    cwd: &str,
    tool_count: usize,
    skill_count: usize,
    context_length: Option<usize>,
    session_id: Option<&str>,
) {
    let accent = Style::new().yellow().bold();
    let dim = Style::new().dim();
    let cyan = Style::new().cyan();
    let _green = Style::new().green();

    // Print logo if terminal is wide enough
    if let Ok((width, _)) = crossterm::terminal::size() {
        if width >= 95 {
            println!();
            println!("{}", accent.apply_to(HERMEZ_AGENT_LOGO));
            println!();
        }
    }

    // Title / version
    println!();
    println!("{}", accent.apply_to(format_banner_version_label()));
    println!();

    // Model + context
    let model_short = model.split('/').next_back().unwrap_or(model);
    let model_short = if model_short.ends_with(".gguf") {
        model_short.strip_suffix(".gguf").unwrap_or(model_short)
    } else {
        model_short
    };
    let model_short = if model_short.len() > 28 {
        &model_short[..25]
    } else {
        model_short
    };

    let ctx_str = context_length
        .map(|cl| format!(" · {} context", dim.apply_to(format_context_length(cl))))
        .unwrap_or_default();

    println!(
        "  {} {} · Nous Research",
        cyan.apply_to(model_short),
        ctx_str
    );
    println!("  {}", dim.apply_to(cwd));

    if let Some(sid) = session_id {
        println!("  {} {}", dim.apply_to("Session:"), sid);
    }

    println!();

    // Summary line
    let mut parts = vec![
        format!("{} tools", tool_count),
        format!("{} skills", skill_count),
    ];
    parts.push("/help for commands".to_string());
    println!("  {}", dim.apply_to(parts.join(" · ")));

    // Update check
    if let Some(behind) = check_for_updates() {
        if behind > 0 {
            let commits_word = if behind == 1 { "commit" } else { "commits" };
            println!();
            println!(
                "  {} {} behind",
                Style::new().yellow().bold().apply_to(format!("⚠ {behind} {commits_word}")),
                dim.apply_to("— run `hermez update` to update")
            );
        }
    }

    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_context_length() {
        assert_eq!(format_context_length(500), "500");
        assert_eq!(format_context_length(128000), "128K");
        assert_eq!(format_context_length(1_048_576), "1M");
        assert_eq!(format_context_length(2_000_000), "2M");
    }

    #[test]
    fn test_banner_version_label_without_git() {
        let label = format_banner_version_label();
        assert!(label.contains("Hermez Agent"));
        assert!(label.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn test_logo_not_empty() {
        assert!(!HERMEZ_AGENT_LOGO.is_empty());
        assert!(!HERMEZ_CADUCEUS.is_empty());
    }

    #[test]
    fn test_print_welcome_banner_does_not_panic() {
        print_welcome_banner(
            "anthropic/claude-opus-4-6",
            "/home/user",
            42,
            12,
            Some(200_000),
            Some("test-session-123"),
        );
    }

    #[test]
    fn test_print_welcome_banner_minimal() {
        print_welcome_banner(
            "gpt-4",
            "/tmp",
            0,
            0,
            None,
            None,
        );
    }
}
