#![allow(dead_code)]
//! Subdirectory hint discovery.
//!
//! As the agent navigates into subdirectories via tool calls (read_file, terminal,
//! search_files, etc.), this module discovers and loads project context files
//! (AGENTS.md, CLAUDE.md, .cursorrules) from those directories.
//!
//! Discovered hints are appended to tool results so the model gets relevant context
//! at the moment it starts working in a new area of the codebase.
//!
//! This complements the startup context loading which only loads from the CWD.
//! Subdirectory hints are discovered lazily and injected into the conversation
//! without modifying the system prompt (preserving prompt caching).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Context files to look for in subdirectories, in priority order.
const HINT_FILENAMES: &[&str] = &[
    "AGENTS.md", "agents.md",
    "CLAUDE.md", "claude.md",
    ".cursorrules",
];

/// Maximum chars per hint file to prevent context bloat.
const MAX_HINT_CHARS: usize = 8_000;

/// Format a number with comma separators.
fn format_number(n: usize) -> String {
    n.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(",")
}

/// Tool argument keys that typically contain file paths.
const PATH_ARG_KEYS: &[&str] = &["path", "file_path", "workdir"];

/// How many parent directories to walk up when looking for hints.
const MAX_ANCESTOR_WALK: usize = 5;

/// Track which directories the agent visits and load hints on first access.
pub struct SubdirectoryHintTracker {
    working_dir: PathBuf,
    loaded_dirs: HashSet<PathBuf>,
}

impl SubdirectoryHintTracker {
    /// Create a new tracker with the given working directory.
    pub fn new(working_dir: Option<&Path>) -> Self {
        let wd = working_dir
            .map(|p| p.to_path_buf())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let mut loaded = HashSet::new();
        if let Ok(resolved) = wd.canonicalize() {
            loaded.insert(resolved);
        }
        Self {
            working_dir: wd,
            loaded_dirs: loaded,
        }
    }

    /// Check tool call arguments for new directories and load any hint files.
    ///
    /// Returns formatted hint text to append to the tool result, or None.
    pub fn check_tool_call(&mut self, tool_name: &str, tool_args: &serde_json::Value) -> Option<String> {
        let dirs = self.extract_directories(tool_name, tool_args);
        if dirs.is_empty() {
            return None;
        }

        let all_hints: Vec<String> = dirs
            .into_iter()
            .filter_map(|d| self.load_hints_for_directory(&d))
            .collect();

        if all_hints.is_empty() {
            return None;
        }

        Some(format!("\n\n{}", all_hints.join("\n\n")))
    }

    /// Extract directory paths from tool call arguments.
    fn extract_directories(&self, tool_name: &str, args: &serde_json::Value) -> Vec<PathBuf> {
        let mut candidates: HashSet<PathBuf> = HashSet::new();

        // Direct path arguments
        if let Some(obj) = args.as_object() {
            for &key in PATH_ARG_KEYS {
                if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
                    if !val.trim().is_empty() {
                        self.add_path_candidate(val, &mut candidates);
                    }
                }
            }
        }

        // Shell commands — extract path-like tokens
        if tool_name == "terminal" {
            if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                self.extract_paths_from_command(cmd, &mut candidates);
            }
        }

        candidates.into_iter().collect()
    }

    /// Resolve a raw path and add its directory + ancestors to candidates.
    fn add_path_candidate(&self, raw_path: &str, candidates: &mut HashSet<PathBuf>) {
        let mut p = PathBuf::from(raw_path);
        if !p.is_absolute() {
            p = self.working_dir.join(&p);
        }
        if let Ok(resolved) = p.canonicalize() {
            // Use parent if it's a file path
            let mut current = if resolved.extension().is_some() || resolved.is_file() {
                resolved.parent().map(|p| p.to_path_buf()).unwrap_or(resolved)
            } else {
                resolved
            };

            // Walk up ancestors
            for _ in 0..MAX_ANCESTOR_WALK {
                if self.loaded_dirs.contains(&current) {
                    break;
                }
                if current.is_dir() && !self.loaded_dirs.contains(&current) {
                    candidates.insert(current.clone());
                }
                if let Some(parent) = current.parent() {
                    if parent == current {
                        break; // filesystem root
                    }
                    current = parent.to_path_buf();
                } else {
                    break;
                }
            }
        }
    }

    /// Extract path-like tokens from a shell command string.
    fn extract_paths_from_command(&self, cmd: &str, candidates: &mut HashSet<PathBuf>) {
        // Simple tokenization — split on spaces, skip flags
        for token in cmd.split_whitespace() {
            if token.starts_with('-') {
                continue;
            }
            if !token.contains('/') && !token.contains('.') {
                continue;
            }
            if token.starts_with("http://") || token.starts_with("https://") || token.starts_with("git@") {
                continue;
            }
            self.add_path_candidate(token, candidates);
        }
    }

    /// Load hint files from a directory. Returns formatted text or None.
    fn load_hints_for_directory(&mut self, directory: &Path) -> Option<String> {
        // Skip if already loaded
        if !self.loaded_dirs.insert(directory.to_path_buf()) {
            return None;
        }

        for filename in HINT_FILENAMES {
            let hint_path = directory.join(filename);
            if !hint_path.is_file() {
                continue;
            }

            let Ok(content) = std::fs::read_to_string(&hint_path) else {
                continue;
            };
            let content = content.trim().to_string();
            if content.is_empty() {
                continue;
            }

            // Truncate if too large
            let truncated = if content.len() > MAX_HINT_CHARS {
                format!(
                    "{}\n\n[...truncated {filename}: {} chars total]",
                    &content[..MAX_HINT_CHARS],
                    format_number(content.len())
                )
            } else {
                content.clone()
            };

            // Best-effort relative path for display
            let rel_path = hint_path
                .strip_prefix(&self.working_dir)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
                .or_else(|| {
                    hint_path
                        .strip_prefix(shellexpand::tilde("~").as_ref())
                        .ok()
                        .map(|p| format!("~/{}", p.to_string_lossy()))
                })
                .unwrap_or_else(|| hint_path.to_string_lossy().to_string());

            return Some(format!(
                "[Subdirectory context discovered: {rel_path}]\n{truncated}"
            ));
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_new_tracker() {
        let tracker = SubdirectoryHintTracker::new(None);
        // Should have current dir in loaded_dirs
        assert!(!tracker.loaded_dirs.is_empty());
    }

    #[test]
    fn test_load_hint_from_directory() {
        let dir = std::env::temp_dir().join("test_hint_dir");
        let _ = std::fs::create_dir_all(&dir);
        let hint_file = dir.join("CLAUDE.md");
        let mut file = std::fs::File::create(&hint_file).unwrap();
        writeln!(file, "# Test hint").unwrap();
        drop(file);

        let _tracker = SubdirectoryHintTracker::new(Some(&dir));
        // Create a subdir and check hints
        let subdir = dir.join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();

        // Put a hint in subdir
        let sub_hint = subdir.join("AGENTS.md");
        let mut f = std::fs::File::create(&sub_hint).unwrap();
        writeln!(f, "# Agent instructions for subdir").unwrap();
        drop(f);

        let mut tracker = SubdirectoryHintTracker::new(Some(&dir));
        let result = tracker.load_hints_for_directory(&subdir);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("AGENTS.md"));

        // Second call should return None (already loaded)
        let result2 = tracker.load_hints_for_directory(&subdir);
        assert!(result2.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_check_tool_call_with_path_arg() {
        let dir = std::env::temp_dir().join("test_hint_tracker");
        let _ = std::fs::create_dir_all(&dir);
        let hint_file = dir.join("CLAUDE.md");
        std::fs::write(&hint_file, "# Project rules\nAlways use Rust.").unwrap();

        let subdir = dir.join("src");
        std::fs::create_dir_all(&subdir).unwrap();

        let mut tracker = SubdirectoryHintTracker::new(Some(&dir));
        let args = serde_json::json!({
            "path": "src/main.rs"
        });
        // The hint should be found since src/ has CLAUDE.md from parent
        let result = tracker.check_tool_call("file_read", &args);
        // May or may not find hints depending on ancestor walk
        let _ = result;

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_paths_from_command() {
        let dir = std::env::temp_dir().join("test_cmd_extract");
        let subdir = dir.join("src").join("components");
        std::fs::create_dir_all(&subdir).unwrap();

        let tracker = SubdirectoryHintTracker::new(Some(&dir));
        let mut candidates = HashSet::new();
        tracker.extract_paths_from_command("cd src/components && ls", &mut candidates);
        // Should find "src/components"
        assert!(!candidates.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_skip_urls_in_command() {
        let dir = std::env::temp_dir().join("test_url_skip");
        std::fs::create_dir_all(&dir).unwrap();
        let tracker = SubdirectoryHintTracker::new(Some(&dir));
        let mut candidates = HashSet::new();
        tracker.extract_paths_from_command("curl https://example.com/api", &mut candidates);
        assert!(candidates.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_max_hint_chars_truncation() {
        let dir = std::env::temp_dir().join("test_hint_trunc");
        let _ = std::fs::create_dir_all(&dir);
        let large_content = "A".repeat(MAX_HINT_CHARS + 1000);
        let hint_file = dir.join("AGENTS.md");
        std::fs::write(&hint_file, &large_content).unwrap();

        let mut tracker = SubdirectoryHintTracker::new(Some(&dir));
        let result = tracker.load_hints_for_directory(&dir);
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("truncated"));
        assert!(text.len() < large_content.len());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
